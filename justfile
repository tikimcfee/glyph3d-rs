# glyph3d-native tasks

# Default target
default: check

# Build the binary
build:
	cargo glyph build

# Run everything; nonzero if anything is wrong
check:
	cargo glyph test

# Only what you changed: engine | rust | render | corpus
check-scoped scope:
	cargo glyph test {{scope}}

# Assert currency instead of building it
verify:
	cargo glyph test --frozen

# Applies each mutation declared in build.toml
prove:
	cargo glyph prove

# Fast unit & integration test runner via cargo-nextest
test:
	CI=1 cargo nextest run --workspace

# Record sampling profile of repo loading with samply
profile file="/Users/lugo/localdev/viz-web/glyph3d-js":
	@mkdir -p out
	samply record --save-only -o out/profile-glyph3d-js.json.gz target/release/glyph3d-native --load-repo {{file}} --repo-scan-only

# Launch interactive Firefox Profiler UI on the recorded profile
profile-view profile="out/profile-glyph3d-js.json.gz":
	samply load {{profile}}

# Generators
gen-trie:
	python3 tools/gen_real_trie.py

gen-schema:
	python3 tools/gen_schema.py

check-gen:
	python3 tools/gen_real_trie.py --verify-only
	python3 tools/gen_schema.py --check

emoji-inventory:
	python3 tools/emoji_inventory.py

gen-emoji-sheet:
	python3 tools/gen_emoji_sheet.py

# Prebake or refresh the derived atlas mip cache
bake-atlas-cache:
	@rm -f assets/atlas/emoji-sheet.cache
	cargo run --release -p glyph3d-native -- --screenshot /tmp/atlas-cache-bake.png
	@rm -f /tmp/atlas-cache-bake.png
	@echo "[ok] Baked assets/atlas/emoji-sheet.cache"

build-native:
	cd native && cargo build --release

