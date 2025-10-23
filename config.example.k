# Thalamus Configuration Example
# Copy this to config.k and customize for your environment
# KCL provides type-safe configuration with validation and hot-reload

# Import shared schemas
import pkg

# Server configuration
server = pkg.ServerConfig {
    host = "0.0.0.0"
    port = 3000
    workers = 4  # Auto-detected on runtime if not specified
}

# Database configuration
# Use environment variables or devenv-provided DATABASE_URL
database = pkg.DatabaseConfig {
    url = "postgresql://postgres:postgres@localhost:5432/thalamus"
    max_connections = 20
    min_connections = 2
    pool_timeout = "30s"
    idle_timeout = "600s"
    max_lifetime = "1800s"
}

# Backend definitions
backends = {
    # OpenAI backend
    openai = pkg.BackendConfig {
        endpoints = [
            pkg.EndpointConfig {
                url = "https://api.openai.com/v1"
                capacity = 100
                models = ["gpt-4o", "gpt-4o-mini", "gpt-4-turbo", "gpt-3.5-turbo"]
                tags = ["openai", "production", "high-capacity"]
                weight = 10
            }
        ]
        auth = pkg.AuthConfig {
            type = "bearer_token"
            token = "${OPENAI_API_KEY}"
        }
        health_check = pkg.HealthCheckConfig {
            enabled = True
            interval = "30s"
            timeout = "5s"
            unhealthy_threshold = 3
            healthy_threshold = 2
        }
        retry_config = pkg.RetryConfig {
            max_retries = 3
            initial_delay = "1s"
            max_delay = "10s"
            exponential_backoff = True
        }
        timeout = "60s"
    }

    # Local Ollama backend
    ollama = pkg.BackendConfig {
        endpoints = [
            pkg.EndpointConfig {
                url = "http://localhost:11434/v1"
                capacity = 10
                models = ["llama3.2:latest", "qwen2.5:7b", "mistral:7b"]
                currently_loaded_models = []  # Detected at runtime
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

    # Anthropic backend
    anthropic = pkg.BackendConfig {
        endpoints = [
            pkg.EndpointConfig {
                url = "https://api.anthropic.com/v1"
                capacity = 50
                models = ["claude-3-5-sonnet-20241022", "claude-3-5-haiku-20241022", "claude-3-opus-20240229"]
                tags = ["anthropic", "production"]
                weight = 8
            }
        ]
        auth = pkg.AuthConfig {
            type = "api_key"
            header = "x-api-key"
            token = "${ANTHROPIC_API_KEY}"
        }
        health_check = pkg.HealthCheckConfig {
            enabled = True
            interval = "30s"
            timeout = "5s"
        }
        retry_config = pkg.RetryConfig {
            max_retries = 2
            initial_delay = "2s"
        }
        timeout = "60s"
    }
}

# Routing configuration
routing = pkg.RoutingConfig {
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
observability = pkg.ObservabilityConfig {
    tracing = pkg.TracingConfig {
        enabled = True
        level = "info"
        format = "json"
        otlp_endpoint = "http://localhost:4317"  # Optional: OpenTelemetry endpoint
        sample_rate = 1.0  # 100% sampling for development
    }

    metrics = pkg.MetricsConfig {
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
cache = pkg.CacheConfig {
    enabled = True
    redis_url = "redis://localhost:6379"
    default_ttl = "300s"
    max_memory = "1GB"
    key_prefix = "thalamus:"
}

# Optional: Rate limiting configuration
rate_limiting = pkg.RateLimitConfig {
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
