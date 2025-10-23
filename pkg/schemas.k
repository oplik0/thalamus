# Thalamus Configuration Schemas
# Defines type-safe configuration structures with validation

schema ServerConfig:
    """Server configuration for the Thalamus HTTP server"""
    host: str = "0.0.0.0"
    port: int = 3000
    workers?: int  # Optional, auto-detected if not specified

    check:
        1 <= port <= 65535, "Port must be between 1 and 65535"
        workers == None or workers > 0, "Workers must be positive"

schema DatabaseConfig:
    """Database connection configuration"""
    url: str
    max_connections: int = 10
    min_connections: int = 1
    pool_timeout: str = "30s"
    idle_timeout: str = "600s"
    max_lifetime: str = "1800s"

    check:
        max_connections > 0, "Max connections must be positive"
        min_connections > 0 and min_connections <= max_connections, \
            "Min connections must be positive and <= max connections"
        url.startswith("postgres://") or url.startswith("postgresql://"), \
            "Only PostgreSQL databases are supported"

schema AuthConfig:
    """Authentication configuration for backend connections"""
    type: "none" | "bearer_token" | "api_key" | "basic" | "custom"
    token?: str
    username?: str
    password?: str
    header?: str  # Custom header name for api_key type

    check:
        type == "none" or token != None or (username != None and password != None), \
            "Authentication credentials required when type is not 'none'"

schema HealthCheckConfig:
    """Health check configuration for backends"""
    enabled: bool = True
    interval: str = "30s"
    timeout: str = "5s"
    endpoint?: str = "/health"
    unhealthy_threshold: int = 3
    healthy_threshold: int = 2

    check:
        unhealthy_threshold > 0, "Unhealthy threshold must be positive"
        healthy_threshold > 0, "Healthy threshold must be positive"

schema EndpointConfig:
    """Individual backend endpoint configuration"""
    url: str
    capacity: int = 10
    models: [str]
    infer_models: bool = True
    currently_loaded_models?: [str] = []
    model_loading_aware: bool = False
    tags: [str] = []
    weight: int = 1  # For weighted load balancing

    check:
        capacity > 0, "Capacity must be positive"
        weight > 0, "Weight must be positive"
        len(models) > 0 or infer_models, "At least one model must be specified or infer_models must be enabled"
        url.startswith("http://") or url.startswith("https://"), \
            "URL must start with http:// or https://"

schema RetryConfig:
    """Retry configuration for failed requests"""
    max_retries: int = 3
    initial_delay: str = "1s"
    max_delay: str = "10s"
    exponential_backoff: bool = True
    retry_on_timeout: bool = True

    check:
        max_retries >= 0, "Max retries cannot be negative"

schema BackendConfig:
    """Backend configuration with endpoints, auth, and health checks"""
    endpoints: [EndpointConfig]
    auth: AuthConfig
    health_check?: HealthCheckConfig
    retry_config?: RetryConfig
    timeout: str = "30s"

    check:
        len(endpoints) > 0, "At least one endpoint must be defined"

schema StrategyConfig:
    """Routing strategy configuration"""
    name: "model_aware_least_busy" | "round_robin" | "random" | "weighted" | "least_connections"
    prefer_loaded_models: bool = True
    consider_queue_depth: bool = True
    fallback_strategy: str = "round_robin"

    check:
        fallback_strategy in ["round_robin", "random", "weighted"], \
            "Fallback strategy must be one of: round_robin, random, weighted"

schema QueueConfig:
    """Priority queue configuration"""
    priority: int
    max_queue_size: int
    timeout: str

    check:
        priority > 0, "Priority must be positive"
        max_queue_size > 0, "Max queue size must be positive"

schema RoutingConfig:
    """Routing and queue configuration"""
    strategy: StrategyConfig
    priority_queues: {str: QueueConfig}
    default_queue: str = "realtime"

    check:
        len(priority_queues) > 0, "At least one priority queue must be defined"
        default_queue in priority_queues, \
            "Default queue must exist in priority_queues"

schema TracingConfig:
    """Distributed tracing configuration"""
    enabled: bool = True
    level: "trace" | "debug" | "info" | "warn" | "error" = "info"
    format: "pretty" | "json" | "compact" = "json"
    otlp_endpoint?: str
    sample_rate: float = 1.0

    check:
        0.0 <= sample_rate <= 1.0, "Sample rate must be between 0.0 and 1.0"

schema MetricsConfig:
    """Metrics collection configuration"""
    enabled: bool = True
    prometheus_endpoint: str = "/metrics"
    collection_interval: str = "10s"
    include_per_backend: bool = True
    include_per_model: bool = True

schema ObservabilityConfig:
    """Observability stack configuration"""
    tracing: TracingConfig
    metrics: MetricsConfig
    logging_per_team?: {str: str}  # Team ID -> log level mapping

schema CacheConfig:
    """Redis cache configuration"""
    enabled: bool = True
    redis_url: str
    default_ttl: str = "300s"
    max_memory?: str
    key_prefix: str = "thalamus:"

    check:
        redis_url.startswith("redis://") or redis_url.startswith("rediss://"), \
            "Redis URL must start with redis:// or rediss://"

schema RateLimitConfig:
    """Rate limiting configuration"""
    enabled: bool = True
    algorithm: "token_bucket" | "sliding_window" = "token_bucket"
    default_requests_per_minute: int = 60
    burst_size: int = 10
    per_team_limits?: {str: int}  # Team ID -> RPM mapping

    check:
        default_requests_per_minute > 0, "Requests per minute must be positive"
        burst_size > 0, "Burst size must be positive"

# Root configuration schema
schema ThalamusConfig:
    """Root configuration for Thalamus LLM Router"""
    server: ServerConfig
    database: DatabaseConfig
    backends: {str: BackendConfig}
    routing: RoutingConfig
    observability: ObservabilityConfig
    cache?: CacheConfig
    rate_limiting?: RateLimitConfig

    check:
        len(backends) > 0, "At least one backend must be configured"
