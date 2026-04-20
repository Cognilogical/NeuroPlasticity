#!/bin/bash
set -e

echo "Building neuro-rust-testbed..."
podman build -t neuro-rust-testbed -f images/neuro-rust-testbed.Containerfile images/

echo "Building neuro-node-testbed..."
podman build -t neuro-node-testbed -f images/neuro-node-testbed.Containerfile images/

echo "Building neuro-agent-testbed..."
podman build -t neuro-agent-testbed -f images/neuro-agent-testbed.Containerfile images/

echo "All images built successfully."
