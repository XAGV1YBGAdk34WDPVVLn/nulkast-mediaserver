.PHONY: test run build deploy logs

MEDIA_DIR ?= ./media
PORT ?= 8275

test:
	cargo test

run:
	MEDIA_DIR=$(MEDIA_DIR) PORT=$(PORT) cargo run

build:
	cargo build --release

deploy:
	python3 scripts/mediaserver_ctl.py deploy

logs:
	python3 scripts/mediaserver_ctl.py logs --tail 100
