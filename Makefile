# Build + run the claude-seller (dev's base image + the ACP adapter).
# For testers of the mobee seller — not a merge target.
#
#   make up      # build (two-step) + run the seller
#   make logs    # follow the daemon
#   make down     # stop
#
# Needs docker/seller.env with ANTHROPIC_API_KEY (see docker/seller.env.example).

IMAGE   ?= mobee-seller-shim
BASE    ?= mobee-base
COMPOSE  = docker compose -f docker-compose.claude-shim.yml

.PHONY: build up down logs base

base:                ## build dev's base image (mobee binary; no agent)
	docker build -f Dockerfile -t $(BASE) .

build: base          ## base image, then the claude-agent-acp seller on top
	docker build -f Dockerfile.claude-shim --build-arg BASE_IMAGE=$(BASE) -t $(IMAGE) .

up: build            ## build + run (uses the pre-built image; no compose build)
	$(COMPOSE) up -d --no-build

logs:                ## follow the seller daemon
	$(COMPOSE) logs -f seller

down:                ## stop (keeps the volume / identity)
	$(COMPOSE) down
