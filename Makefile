PLUGIN_DIR := $(HOME)/.docker/cli-plugins

.PHONY: build install uninstall clean test

build:
	cargo build --release

install: build
	mkdir -p $(PLUGIN_DIR)
	cp target/release/docker-dbake $(PLUGIN_DIR)/docker-dbake
	chmod +x $(PLUGIN_DIR)/docker-dbake
	@echo "Installed docker-dbake to $(PLUGIN_DIR)/docker-dbake"
	@echo "Verify: docker dbake --help"

uninstall:
	rm -f $(PLUGIN_DIR)/docker-dbake

clean:
	cargo clean

test:
	cargo test
