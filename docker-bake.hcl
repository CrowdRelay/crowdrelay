variable "API_IMAGE" {
  default = "crowdrelay-api:local"
}

variable "WORKER_IMAGE" {
  default = "crowdrelay-worker:local"
}

# Publishing supplies the immutable source identity; local bakes leave both
# empty so the dependency layers stay reusable across source-only commits.
variable "CROWDRELAY_GIT_SHA" {
  default = ""
}

variable "CROWDRELAY_BUILD_TIMESTAMP" {
  default = ""
}

variable "CACHE_SCOPE" {
  default = ""
}

group "default" {
  targets = ["api", "worker"]
}

target "_common" {
  context    = "."
  dockerfile = "Dockerfile"
  args = {
    CROWDRELAY_GIT_SHA         = CROWDRELAY_GIT_SHA
    CROWDRELAY_BUILD_TIMESTAMP = CROWDRELAY_BUILD_TIMESTAMP
  }
  labels = {
    "org.opencontainers.image.source"   = "https://github.com/wojciechbator/crowdrelay"
    "org.opencontainers.image.revision" = CROWDRELAY_GIT_SHA
    "org.opencontainers.image.licenses" = "Apache-2.0"
  }
  # Both runtime targets descend from one `builder` stage. Baking them together
  # compiles that stage once inside a single build graph, instead of running two
  # buildx invocations that each re-export a mode=max Rust cache for it.
  cache-from = CACHE_SCOPE == "" ? [] : ["type=gha,scope=${CACHE_SCOPE}"]
  cache-to   = CACHE_SCOPE == "" ? [] : ["type=gha,mode=max,scope=${CACHE_SCOPE}"]
}

target "api" {
  inherits = ["_common"]
  target   = "api"
  tags     = ["${API_IMAGE}"]
  labels = {
    "org.opencontainers.image.title" = "crowdrelay-api"
  }
}

target "worker" {
  inherits = ["_common"]
  target   = "worker"
  tags     = ["${WORKER_IMAGE}"]
  labels = {
    "org.opencontainers.image.title" = "crowdrelay-worker"
  }
}
