# Usage

## Ress

- generate a JWT token:  `./docker/ress/generate-jwt.sh`
- Build ress image locally: `docker build -t ress:local -f docker/ress/Dockerfile .`
- Start ress node: `docker compose -f docker/docker-compose.yml -f docker/lighthouse.yml up -d`
- View ress log: `docker compose -f docker/docker-compose.yml -f docker/lighthouse.yml logs -f ress`
