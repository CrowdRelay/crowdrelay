variable "API_IMAGE" {
  default = "crowdrelay-api:local"
}

variable "WORKER_IMAGE" {
  default = "crowdrelay-worker:local"
}

group "default" {
  targets = ["api", "worker"]
}

target "_common" {
  context = "."
}

target "api" {
  inherits = ["_common"]
  target   = "api"
  tags     = ["${API_IMAGE}"]
}

target "worker" {
  inherits = ["_common"]
  target   = "worker"
  tags     = ["${WORKER_IMAGE}"]
}
