.PHONY: default

hooks:
	cp hooks/* .git/hooks/

test:
	cargo test

bench:
	cargo bench

lint-fix:
	cargo clippy --fix --allow-staged
lint:
	cargo clippy
fmt:
	cargo fmt --all --