# Thalamus Configuration
# KCL provides type-safe configuration with validation and hot-reload
# Use named profiles (e.g., default, dev, prod) to configure different environments

# Import shared schemas
import pkg

# Default profile - used when no profile is specified
default = pkg.ThalamusConfig {
    server = {
        host = "localhost"
        port = 3000
        workers = 4
        base_url = "http://localhost:3000"
    }

    # Database configuration
    # Use environment variables or devenv-provided DATABASE_URL
    database = {
        url = "postgresql://opliko@localhost:5432/thalamus"
        max_connections = 20
        min_connections = 2
        pool_timeout = "30s"
        idle_timeout = "600s"
        max_lifetime = "1800s"
    }

    # Security configuration
    # Set these via environment variables or replace with your actual secrets
    security = {
        api_key_secret = "131ed4de51d4dec800105d289be32a7f15d42b946b4b662356834f3692bfb267"
        paseto_secret_key = "c48cd307d55ac1f7d4e96530b26045d9"
        opaque_server_setup = "dev"
    }

    # Backend definitions
    backends = {
        # OpenAI backend
        # openai = pkg.BackendConfig {
        #     endpoints = [
        #         pkg.EndpointConfig {
        #             url = "https://api.openai.com/v1"
        #             capacity = 100
        #             models = ["gpt-4o", "gpt-4o-mini", "gpt-4-turbo", "gpt-3.5-turbo"]
        #             tags = ["openai", "production", "high-capacity"]
        #             weight = 10
        #         }
        #     ]
        #     auth = pkg.AuthConfig {
        #         type = "bearer_token"
        #         token = "${OPENAI_API_KEY}"
        #     }
        #     health_check = pkg.HealthCheckConfig {
        #         enabled = True
        #         interval = "30s"
        #         timeout = "5s"
        #         unhealthy_threshold = 3
        #         healthy_threshold = 2
        #     }
        #     retry_config = pkg.RetryConfig {
        #         max_retries = 3
        #         initial_delay = "1s"
        #         max_delay = "10s"
        #         exponential_backoff = True
        #     }
        #     timeout = "60s"
        # }

        # Local Ollama backend
        ollama = pkg.BackendConfig {
            endpoints = [
                pkg.EndpointConfig {
                    url = "http://localhost:11434/v1"
                    capacity = 10
                    models = ["llama3.2:latest", "qwen2.5:7b", "mistral:7b"]
                    currently_loaded_models = []
                    model_loading_aware = True
                    tags = ["local", "self-hosted", "high-privacy", "cost-free"]
                    weight = 5
                }
            ]
            auth = pkg.AuthConfig {
                type = "none"
            }
            health_check = pkg.HealthCheckConfig {
                enabled = True
                interval = "60s"
                timeout = "10s"
            }
            timeout = "120s"
        }
    }

    # Routing configuration
    routing = {
        strategy = pkg.StrategyConfig {
            name = "model_aware_least_busy"
            prefer_loaded_models = True
            consider_queue_depth = True
            fallback_strategy = "round_robin"
        }

        priority_queues = {
            realtime = pkg.QueueConfig {
                priority = 100
                max_queue_size = 1000
                timeout = "30s"
            }
            batch = pkg.QueueConfig {
                priority = 50
                max_queue_size = 10000
                timeout = "300s"
            }
            low_priority = pkg.QueueConfig {
                priority = 10
                max_queue_size = 5000
                timeout = "600s"
            }
        }

        default_queue = "realtime"
    }

    # Observability configuration
    observability = {
        tracing = {
            enabled = True
            level = "info"
            format = "json"
            otlp_endpoint = "http://localhost:4317"
            sample_rate = 1.0
        }

        metrics = {
            enabled = True
            prometheus_endpoint = "/metrics"
            collection_interval = "10s"
            include_per_backend = True
            include_per_model = True
        }

        # Optional: Per-team logging levels
        # logging_per_team = {
        #     "team-engineering" = "debug"
        #     "team-research" = "info"
        # }
    }

    # Optional: Cache configuration (requires Redis)
    cache = {
        enabled = True
        redis_url = "redis://localhost:6379"
        default_ttl = "300s"
        max_memory = "1GB"
        key_prefix = "thalamus:"
    }

    # Optional: Rate limiting configuration
    rate_limiting = {
        enabled = True
        algorithm = "token_bucket"
        default_requests_per_minute = 60
        burst_size = 10

        # Optional: Per-team limits
        # per_team_limits = {
        #     "team-engineering" = 1000
        #     "team-research" = 500
        # }
    }

    oauth_providers = [
        {
            name = "github"
            client_id = "Ov23liHhfBrOonqVXEyr"
            client_secret = "3aedd6f3386dafb8be72841e2a4e876aa494155d"
            provider_type = "github"
            scopes = ["read:user", "user:email"]
            team_mapping = {

            }
        }
    ]
}

# Alias for backwards compatibility
dev = default
