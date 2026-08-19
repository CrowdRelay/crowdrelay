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

# Registry reference for the shared BuildKit cache. Empty for local bakes.
variable "CACHE_REF" {
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
  #
  # The cache lives in the registry, not the Actions cache. mode=max is what
  # keeps the `cargo chef cook` layer reusable, and that export is large enough
  # that the 10 GB Actions cache evicted it between runs. GHCR keeps it.
  # image-manifest/oci-mediatypes are required for GHCR to accept the manifest.
  cache-from = CACHE_REF == "" ? [] : ["type=registry,ref=${CACHE_REF}"]
  cache-to   = CACHE_REF == "" ? [] : ["type=registry,ref=${CACHE_REF},mode=max,image-manifest=true,oci-mediatypes=true"]
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
