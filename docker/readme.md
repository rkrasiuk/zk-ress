# Usage

## ZkRess

## Step 1

- Generate a JWT token:  `./docker/ress/generate-jwt.sh` for CL/EL communication

## Step 2

Build the docker image for zk-ress locally: `docker build -t ress:local -f docker/ress/Dockerfile .`

## Step 3

Startup up the zk-ress node: `docker compose -f docker/docker-compose.yml up -d`

> This assumes that you have added the enode for a reth node that has zk-ress enabled in the docker-compose.yml file. See trusted-peers.

## Logging

- View the logs with: `docker compose -f docker/docker-compose.yml -f docker/lighthouse.yml logs -f ress`
