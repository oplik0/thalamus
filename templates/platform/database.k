import regex

# The administrator Secret's url must connect to an existing dedicated database.
# The runtime Secret's url must connect to that SAME database using exactly its
# username and password keys. Reserve the runtime role for this application;
# PostgreSQL roles are cluster-wide, and PUBLIC/database grants are operator-owned.
# Use a database owner with CREATEROLE and authority to administer the application
# role; an existing role also requires ADMIN OPTION and permission to SET ROLE to
# it. The client image must contain /bin/sh and psql 15 or newer. Protect the
# connection with TLS and configure statement/error logging not to retain secrets.
# Optionally mount caSecret key ca.crt at /var/run/thalamus/db-ca/ca.pem for
# sslmode=verify-full. An sslrootcert in the admin URI overrides PGSSLROOTCERT;
# if specified, it must identify that same mounted CA path.
# psql must receive the admin URI as its database argument, so processes able to
# inspect its arguments can see URI credentials; restrict access to the pod/node.
#
# A revision names both the immutable SQL ConfigMap and the Job. Change it after
# rotating Secrets or changing the image, SQL, or other Job configuration; an
# already-completed Job will not run again. Jobs have no TTL so their completion
# status remains available. Wait for this Job, then start the server and let it
# apply its serialized, checksum-verified migrations before starting workers.
schema Database:
    namespace: str
    postgresImage: str
    adminSecret: str
    runtimeSecret: str
    bootstrapRevision: str
    caSecret: str = ""
    _adminSecretParts: [str] = adminSecret.split(".")
    _runtimeSecretParts: [str] = runtimeSecret.split(".")
    _caSecretParts: [str] = caSecret.split(".")

    items: [any] = [{
        apiVersion: "v1"
        kind: "ConfigMap"
        metadata: {name: "thalamus-db-bootstrap-${bootstrapRevision}", namespace: namespace}
        immutable: True
        data: {
            "bootstrap.sql": r"""
                \set ON_ERROR_STOP on
                \getenv app_user APP_USERNAME
                \getenv app_password APP_PASSWORD

                BEGIN;
                SET LOCAL search_path = pg_catalog;
                SELECT pg_advisory_xact_lock(pg_catalog.hashtext('thalamus-db-bootstrap'));

                -- Empty passwords become NULL in PostgreSQL. Reject names that
                -- PostgreSQL would silently truncate or that name the admin.
                SELECT pg_catalog.length(:'app_user') > 0
                   AND pg_catalog.octet_length(:'app_user') <= pg_catalog.current_setting('max_identifier_length')::integer
                   AND pg_catalog.length(:'app_password') > 0
                   AND :'app_user' <> current_user AS valid_credentials
                \gset
                \if :valid_credentials
                \else
                DO $body$
                BEGIN
                    RAISE EXCEPTION 'invalid application role credentials';
                END;
                $body$;
                \endif

                -- Never strip attributes or memberships from an existing role.
                -- Membership permits SET ROLE even when the role is NOINHERIT.
                SELECT EXISTS (
                    SELECT 1
                    FROM pg_catalog.pg_roles AS r
                    WHERE r.rolname = :'app_user'
                      AND (r.rolsuper OR r.rolcreatedb OR r.rolcreaterole
                           OR r.rolreplication OR r.rolbypassrls OR r.rolinherit
                           OR EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members AS m WHERE m.member = r.oid)
                           OR EXISTS (SELECT 1 FROM pg_catalog.pg_database AS d WHERE d.datdba = r.oid))
                ) AS privileged_role
                \gset
                \if :privileged_role
                DO $body$
                BEGIN
                    RAISE EXCEPTION 'existing application role has privileged attributes, memberships, or database ownership';
                END;
                $body$;
                \endif

                -- ROLE makes the administrator a member of the new role, so a
                -- non-superuser CREATEROLE administrator can create its schema.
                SELECT pg_catalog.format(
                    'CREATE ROLE %I WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS NOINHERIT ROLE %I',
                    :'app_user', current_user)
                WHERE NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = :'app_user')
                \gexec

                -- An existing schema is accepted only when already owned by the
                -- application role; never transfer or replace its ownership.
                SELECT EXISTS (
                    SELECT 1 FROM pg_catalog.pg_namespace AS n
                    JOIN pg_catalog.pg_roles AS r ON r.rolname = :'app_user'
                    WHERE n.nspname = 'thalamus' AND n.nspowner <> r.oid
                ) AS schema_conflict
                \gset
                \if :schema_conflict
                DO $body$
                BEGIN
                    RAISE EXCEPTION 'existing thalamus schema has a different owner';
                END;
                $body$;
                \endif

                -- Without IF NOT EXISTS a concurrent foreign schema creation
                -- fails rather than silently treating the foreign schema as ours.
                SELECT pg_catalog.format('CREATE SCHEMA thalamus AUTHORIZATION %I', :'app_user')
                WHERE NOT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = 'thalamus')
                \gexec

                SELECT pg_catalog.format('ALTER ROLE %I WITH LOGIN PASSWORD %L', :'app_user', :'app_password')
                \gexec
                SELECT pg_catalog.format('GRANT CONNECT ON DATABASE %I TO %I', current_database(), :'app_user')
                \gexec
                SELECT pg_catalog.format(
                    'ALTER ROLE %I IN DATABASE %I SET search_path TO thalamus, pg_catalog',
                    :'app_user', current_database())
                \gexec
                COMMIT;
            """
        }
    }, {
        apiVersion: "batch/v1"
        kind: "Job"
        metadata: {name: "thalamus-db-bootstrap-${bootstrapRevision}", namespace: namespace}
        spec: {
            backoffLimit: 3
            template: {
                spec: {
                    automountServiceAccountToken: False
                    restartPolicy: "Never"
                    securityContext: {
                        runAsNonRoot: True
                        runAsUser: 65532
                        runAsGroup: 65532
                        seccompProfile: {type: "RuntimeDefault"}
                    }
                    containers: [{
                        name: "bootstrap"
                        image: postgresImage
                        command: ["/bin/sh", "-c"]
                        args: [r"""
                            case "$ADMIN_DATABASE_URL" in
                                postgres://?*|postgresql://?*) ;;
                                *) printf '%s\n' 'admin Secret url must be a PostgreSQL URI' >&2; exit 1 ;;
                            esac
                            exec psql -X -q -w -v ON_ERROR_STOP=1 -f /bootstrap/bootstrap.sql "$ADMIN_DATABASE_URL"
                        """]
                        env: [
                            {name: "ADMIN_DATABASE_URL", valueFrom: {secretKeyRef: {name: adminSecret, key: "url"}}},
                            {name: "APP_USERNAME", valueFrom: {secretKeyRef: {name: runtimeSecret, key: "username"}}},
                            {name: "APP_PASSWORD", valueFrom: {secretKeyRef: {name: runtimeSecret, key: "password"}}},
                        ] + ([{name: "PGSSLROOTCERT", value: "/var/run/thalamus/db-ca/ca.pem"}] if caSecret != "" else [])
                        securityContext: {
                            allowPrivilegeEscalation: False
                            readOnlyRootFilesystem: True
                            capabilities: {drop: ["ALL"]}
                        }
                        volumeMounts: [{name: "sql", mountPath: "/bootstrap", readOnly: True}] + ([{name: "db-ca", mountPath: "/var/run/thalamus/db-ca", readOnly: True}] if caSecret != "" else [])
                    }]
                    volumes: [{name: "sql", configMap: {name: "thalamus-db-bootstrap-${bootstrapRevision}"}}] + ([{name: "db-ca", secret: {secretName: caSecret, items: [{key: "ca.crt", path: "ca.pem"}]}}] if caSecret != "" else [])
                }
            }
        }
    }]

    check:
        len(namespace) <= 63 and regex.match(namespace, r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "namespace must be a Kubernetes DNS label"
        regex.match(postgresImage, r"^[^@\s]+@sha256:[0-9a-f]{64}$"), "PostgreSQL client image must have an immutable sha256 digest"
        len(adminSecret) <= 253 and regex.match(adminSecret, r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$") and (all part in _adminSecretParts {len(part) <= 63}), "admin Secret must be a Kubernetes DNS subdomain"
        len(runtimeSecret) <= 253 and regex.match(runtimeSecret, r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$") and (all part in _runtimeSecretParts {len(part) <= 63}), "runtime Secret must be a Kubernetes DNS subdomain"
        caSecret == "" or (len(caSecret) <= 253 and regex.match(caSecret, r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$") and (all part in _caSecretParts {len(part) <= 63})), "CA Secret must be empty or a Kubernetes DNS subdomain"
        adminSecret != runtimeSecret, "admin and runtime Secrets must be distinct"
        len("thalamus-db-bootstrap-") + len(bootstrapRevision) <= 63 and regex.match(bootstrapRevision, r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "bootstrap revision must fit the Job's Kubernetes DNS label"
