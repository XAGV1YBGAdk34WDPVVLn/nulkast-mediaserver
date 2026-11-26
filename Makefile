.PHONY: test test-xbox run build deploy logs

MEDIA_DIR ?= ./media
PORT ?= 8275

test:
	python3 scripts/mediaserver_ctl.py test

test-xbox:
	docker build -q -f tests/Dockerfile.xbox -t nulkast-xbox-harness:latest tests >/dev/null
	docker run --rm --ipc=host -v "$(CURDIR):/work" -w /work nulkast-xbox-harness:latest node /work/tests/xbox_controller_harness.mjs

run:
	MEDIA_DIR=$(MEDIA_DIR) PORT=$(PORT) cargo run

build:
	cargo build --release

deploy:
	python3 scripts/mediaserver_ctl.py deploy

logs:
	python3 scripts/mediaserver_ctl.py logs --tail 100
