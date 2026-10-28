// Schema-driven change-form helpers. The schema and values come from the same
// authenticated, committed GitOps base; no catalog defaults are supplied here.
import type { components } from './api.generated';

type NewChangeArtifact = NonNullable<components['schemas']['CreateChange']['new_artifact']>;

export type ChangeObject = Record<string, unknown>;
export type ChangeField = {
  path: string[];
  label: string;
  kind: 'string' | 'integer' | 'number' | 'boolean' | 'json';
  value: unknown;
  present: boolean;
  required: boolean;
  description?: string;
  options?: unknown[];
  minimum?: number;
  maximum?: number;
  minLength?: number;
  maxLength?: number;
};

export function isChangeObject(value: unknown): value is ChangeObject {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function mergeSchemas(left: ChangeObject, right: ChangeObject): ChangeObject {
  const merged = { ...left, ...right };
  if (isChangeObject(left.properties) && isChangeObject(right.properties)) {
    merged.properties = { ...left.properties, ...right.properties };
  }
  if (Array.isArray(left.required) && Array.isArray(right.required)) {
    merged.required = [...new Set([...left.required, ...right.required])];
  }
  return merged;
}

function localReference(root: unknown, ref: string): unknown {
  if (ref === '#') return root;
  if (!ref.startsWith('#/')) throw new Error(`Unsupported schema reference: ${ref}`);
  let target: unknown = root;
  for (const token of ref.slice(2).split('/')) {
    const key = token.replace(/~1/g, '/').replace(/~0/g, '~');
    if (!isChangeObject(target) || !Object.hasOwn(target, key)) {
      throw new Error(`Schema reference not found: ${ref}`);
    }
    target = target[key];
  }
  return target;
}

function matchesSchema(schema: ChangeObject, value: unknown): boolean {
  if (Object.hasOwn(schema, 'const') && !equalJson(schema.const, value)) return false;
  if (Array.isArray(schema.enum) && !schema.enum.some(option => equalJson(option, value))) return false;
  const types = typeof schema.type === 'string' ? [schema.type] : Array.isArray(schema.type) ? schema.type : [];
  if (!types.length) return true;
  return types.some(type => {
    switch (type) {
      case 'null': return value === null;
      case 'object': return isChangeObject(value);
      case 'array': return Array.isArray(value);
      case 'integer': return typeof value === 'number' && Number.isInteger(value);
      case 'number': return typeof value === 'number';
      case 'boolean': return typeof value === 'boolean';
      case 'string': return typeof value === 'string';
      default: return false;
    }
  });
}

// Resolve local JSON pointers and schema compositions, retaining ambiguous
// unions as JSON fields rather than guessing which variant the user intends.
export function resolveSchema(root: unknown, node: unknown, value?: unknown): ChangeObject {
  function resolve(input: unknown, seen: Set<string>, depth: number): ChangeObject {
    if (!isChangeObject(input)) return {};
    if (depth > 32) throw new Error('Schema nesting is too deep.');
    let result: ChangeObject = {};
    if (typeof input.$ref === 'string') {
      if (seen.has(input.$ref)) throw new Error(`Recursive schema reference: ${input.$ref}`);
      const next = new Set(seen);
      next.add(input.$ref);
      result = resolve(localReference(root, input.$ref), next, depth + 1);
    }
    const own = { ...input };
    delete own.$ref;
    const allOf = Array.isArray(own.allOf) ? own.allOf : [];
    delete own.allOf;
    for (const part of allOf) result = mergeSchemas(result, resolve(part, seen, depth + 1));
    for (const keyword of ['oneOf', 'anyOf'] as const) {
      const variants = own[keyword];
      delete own[keyword];
      if (!Array.isArray(variants)) continue;
      const resolved = variants.map(variant => resolve(variant, seen, depth + 1));
      const matches = value === undefined
        ? resolved.filter(variant => variant.type !== 'null')
        : resolved.filter(variant => matchesSchema(variant, value));
      if (matches.length === 1) result = mergeSchemas(result, matches[0]);
      else if (matches.length > 1) result = mergeSchemas(result, { type: 'ambiguous' });
    }
    return mergeSchemas(result, own);
  }
  return resolve(node, new Set(), 0);
}

export function equalJson(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (Array.isArray(left) && Array.isArray(right)) {
    return left.length === right.length && left.every((value, index) => equalJson(value, right[index]));
  }
  if (isChangeObject(left) && isChangeObject(right)) {
    const keys = Object.keys(left);
    return keys.length === Object.keys(right).length && keys.every(key => Object.hasOwn(right, key) && equalJson(left[key], right[key]));
  }
  return false;
}

/** Reject non-finite numbers and integers that JavaScript cannot preserve exactly. */
export function hasUnsafeChangeNumber(value: unknown): boolean {
  if (typeof value === 'number') return !Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value));
  if (Array.isArray(value)) return value.some(hasUnsafeChangeNumber);
  if (isChangeObject(value)) return Object.values(value).some(hasUnsafeChangeNumber);
  return false;
}

function fieldKind(schema: ChangeObject, value: unknown): ChangeField['kind'] {
  if (value === null) return 'json';
  const declared = Array.isArray(schema.type) ? schema.type.filter(type => type !== 'null') : [schema.type];
  const type = declared.length === 1 ? declared[0] : undefined;
  if (type === 'string' || type === 'integer' || type === 'number' || type === 'boolean') return type;
  if (!type && typeof value === 'string') return 'string';
  if (!type && typeof value === 'boolean') return 'boolean';
  if (!type && typeof value === 'number') return 'number';
  return 'json';
}

function numeric(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

/** Derive editable fields only from the returned deployment schema. */
export function deploymentFields(root: unknown, source: unknown, deploymentIndex: number): ChangeField[] {
  if (!isChangeObject(source) || !Array.isArray(source.deployments) || !isChangeObject(source.deployments[deploymentIndex])) {
    throw new Error('The committed service has no selected deployment.');
  }
  const service = resolveSchema(root, root, source);
  if (!isChangeObject(service.properties) || !Object.hasOwn(service.properties, 'deployments')) {
    throw new Error('The service schema does not define deployments.');
  }
  const deployments = resolveSchema(root, service.properties.deployments, source.deployments);
  if (!deployments.items) throw new Error('The service schema does not define deployment items.');
  const fields: ChangeField[] = [];
  const visit = (node: unknown, value: unknown, path: string[], label: string[], present: boolean, required: boolean, depth: number) => {
    if (depth > 32) throw new Error('Deployment schema nesting is too deep.');
    const schema = resolveSchema(root, node, value);
    const properties = isChangeObject(schema.properties) ? schema.properties : undefined;
    if (properties && (isChangeObject(value) || !present)) {
      const record = isChangeObject(value) ? value : {};
      const requiredNames = Array.isArray(schema.required) ? schema.required : [];
      for (const [key, child] of Object.entries(properties)) {
        const childPresent = Object.hasOwn(record, key);
        visit(child, record[key], [...path, key], [...label, key], childPresent, present && requiredNames.includes(key), depth + 1);
      }
      // Additional map entries are editable only when their schema permits it.
      if (isChangeObject(schema.additionalProperties)) {
        for (const [key, childValue] of Object.entries(record)) {
          if (!Object.hasOwn(properties, key)) visit(schema.additionalProperties, childValue, [...path, key], [...label, key], true, false, depth + 1);
        }
      }
      return;
    }
    fields.push({
      path,
      label: typeof schema.title === 'string' && schema.title ? schema.title : label.join(' / '),
      kind: fieldKind(schema, value),
      value,
      present,
      // KCL applies schema defaults during render even when its export marks
      // a nonoptional defaulted field as required. Never insert that default.
      required: required && !Object.hasOwn(schema, 'default'),
      description: typeof schema.description === 'string' ? schema.description : undefined,
      options: Array.isArray(schema.enum) ? schema.enum : undefined,
      minimum: numeric(schema.minimum),
      maximum: numeric(schema.maximum),
      minLength: numeric(schema.minLength),
      maxLength: numeric(schema.maxLength)
    });
  };
  visit(deployments.items, source.deployments[deploymentIndex], ['deployments', String(deploymentIndex)], [], true, true, 0);
  if (!fields.length) throw new Error('The deployment schema contains no editable fields.');
  return fields;
}

/** Set or remove one nested field without mutating the committed source. */
export function setChangeValue(source: ChangeObject, path: string[], value: unknown, remove = false, base?: ChangeObject): ChangeObject {
  if (!path.length) throw new Error('A field path is required.');
  const copy = structuredClone(source);
  let cursor: unknown = copy;
  let original: unknown = base;
  const ancestors: { parent: ChangeObject; key: string; originallyPresent: boolean }[] = [];
  for (let index = 0; index < path.length; index++) {
    const key = path[index];
    const last = index === path.length - 1;
    if (Array.isArray(cursor)) {
      if (!/^(0|[1-9]\d*)$/.test(key) || Number(key) >= cursor.length) throw new Error('Invalid array index in field path.');
      const item = Number(key);
      if (last) {
        if (remove) cursor.splice(item, 1);
        else cursor[item] = value;
      } else {
        cursor = cursor[item];
      }
      original = Array.isArray(original) ? original[item] : undefined;
    } else if (isChangeObject(cursor)) {
      if (last) {
        if (remove) delete cursor[key];
        else Object.defineProperty(cursor, key, { value, writable: true, configurable: true, enumerable: true });
      } else {
        const originallyPresent = isChangeObject(original) && Object.hasOwn(original, key);
        ancestors.push({ parent: cursor, key, originallyPresent });
        if (!Object.hasOwn(cursor, key)) {
          if (remove) return copy;
          Object.defineProperty(cursor, key, { value: {}, writable: true, configurable: true, enumerable: true });
        }
        cursor = cursor[key];
        original = originallyPresent && isChangeObject(original) ? original[key] : undefined;
      }
    } else {
      throw new Error('The field path traverses a scalar.');
    }
  }
  if (remove && base) {
    for (const { parent, key, originallyPresent } of ancestors.reverse()) {
      const child = parent[key];
      if (!originallyPresent && isChangeObject(child) && Object.keys(child).length === 0) delete parent[key];
    }
  }
  return copy;
}

/** POST /changes replaces each named top-level field, not nested merge patches. */
export function buildChangePatch(source: ChangeObject, draft: ChangeObject): ChangeObject {
  if (!equalJson(source.name, draft.name)) throw new Error('The service name cannot be changed.');
  for (const key of Object.keys(source)) {
    if (!Object.hasOwn(draft, key)) throw new Error(`The API cannot remove the top-level field ${key}.`);
  }
  const patch: ChangeObject = {};
  for (const [key, value] of Object.entries(draft)) {
    if (key !== 'name' && (!Object.hasOwn(source, key) || !equalJson(source[key], value))) {
      Object.defineProperty(patch, key, { value: structuredClone(value), enumerable: true, writable: true, configurable: true });
    }
  }
  return patch;
}

/** Bind a user-supplied artifact to the selected deployment in the replacement patch. */
export function buildNewArtifactPatch(source: ChangeObject, patch: ChangeObject, deploymentIndex: number, artifact: NewChangeArtifact): ChangeObject {
  if (!artifact.uri.startsWith('oci://') || !artifact.uri.slice(6)) throw new Error('The artifact must have an OCI URI.');
  const proposed: ChangeObject = { ...source, ...patch };
  const original = Array.isArray(source.deployments) ? source.deployments[deploymentIndex] : undefined;
  const target = Array.isArray(proposed.deployments) ? proposed.deployments[deploymentIndex] : undefined;
  if (!isChangeObject(original) || !isChangeObject(target) || original.name !== target.name) {
    throw new Error('The selected deployment was changed or reordered in the patch. Select it again before adding the artifact.');
  }
  const path = ['deployments', String(deploymentIndex)];
  let updated = setChangeValue(proposed, [...path, 'artifact'], artifact.name);
  updated = setChangeValue(updated, [...path, 'image'], artifact.uri.slice(6));
  updated = setChangeValue(updated, [...path, 'packagedModel'], true);
  const result = structuredClone(patch);
  Object.defineProperty(result, 'deployments', { value: updated.deployments, enumerable: true, writable: true, configurable: true });
  return result;
}
