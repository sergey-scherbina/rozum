#!/usr/bin/env bash
# Build the rozum-agent container image used by the Docker sandbox backend
# (docs/specs/model-sandbox.md). After this, run an agent jailed in a container:
#
#   ROZUM_SANDBOX_BACKEND=docker rozum launch claude
#
# The tag defaults to what `default_docker_image()` expects (rozum-agent:latest);
# override with ROZUM_SANDBOX_DOCKER_IMAGE and pass it here as $1.
set -euo pipefail

tag="${1:-${ROZUM_SANDBOX_DOCKER_IMAGE:-rozum-agent:latest}}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "Building ${tag} from docker/rozum-agent.Dockerfile ..."
docker build -t "${tag}" -f "${here}/docker/rozum-agent.Dockerfile" "${here}"
echo "Done. Use it with:  ROZUM_SANDBOX_BACKEND=docker rozum launch <agent>"
