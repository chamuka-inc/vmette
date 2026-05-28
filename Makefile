SHELL := /bin/bash

.PHONY: help build assets init guest-bin run shell test clean

help:
	@awk -F':.*##' '/^[a-zA-Z_-]+:.*##/ { printf "  %-12s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

build:         ## cargo build the workspace + codesign vmette
	cargo build --release
	codesign --sign - --force --entitlements entitlements.plist --options=runtime target/release/vmette

assets:        ## Download alpine vmlinuz + initramfs + minirootfs
	bash scripts/fetch-assets.sh
	bash scripts/fetch-alpine-rootfs.sh

init: assets   ## Repack initramfs with vmette's custom /init
	bash scripts/build-initramfs.sh

guest-bin: assets  ## Cross-compile static guest helpers (vsock-send + vsock-runner)
	bash scripts/build-vsock-send.sh

run: init guest-bin   ## Build + sign vmette, boot guest, run default probe
	bash scripts/run.sh

shell: init guest-bin ## Boot guest with no --exec; interactive shell
	bash scripts/run.sh 'exec /bin/sh -l'

test:          ## Run cargo unit tests + end-to-end VM smoke
	cargo test --workspace
	bash tests/run.sh

clean:         ## Remove build artifacts and downloaded assets
	cargo clean
	rm -rf assets
	rm -f tests/fixtures/share/from-guest*
