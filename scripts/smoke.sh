#!/usr/bin/env bash
# End-to-end smoke test for an installed legal-mcp binary and corpus.
#
# Verifies:
#   1. Binary identity (version, expected subcommand surface, no dead commands)
#   2. Corpus health via `stats`
#   3. CLI search (hybrid/vector/keyword modes, type/date/scope filters, recency
#      sort, source routing, seed_text fast path, direct native-ID title hit, include_old)
#   4. CLI retrieval (get-definition, fetch)
#   5. MCP HTTP transport (serve, initialize, tools/list, all tools)
#   6. MCP stdio shim (`legal-mcp mcp`) against the same backend
#
# Read-only against the active immutable corpus; HTTP tests use a fresh port.
#
# Usage:
#   scripts/smoke.sh
#   LEGAL_MCP_BIN=/path/to/legal-mcp scripts/smoke.sh
#   LEGAL_MCP_SKIP_NETWORK=1 scripts/smoke.sh   # skip `fetch` (network)
set -uo pipefail

BIN="${LEGAL_MCP_BIN:-$HOME/.local/bin/legal-mcp}"
SKIP_NETWORK="${LEGAL_MCP_SKIP_NETWORK:-0}"

if [[ ! -x "$BIN" ]]; then
	echo "legal-mcp binary not found or not executable: $BIN" >&2
	echo "Set LEGAL_MCP_BIN or put the binary at \$HOME/.local/bin/legal-mcp." >&2
	exit 2
fi

# ---------------- helpers ----------------

pass=0
fail=0
fail_names=()

c_green=$'\033[32m'
c_red=$'\033[31m'
c_dim=$'\033[2m'
c_reset=$'\033[0m'
if [[ ! -t 1 ]]; then
	c_green=""
	c_red=""
	c_dim=""
	c_reset=""
fi

ok() {
	pass=$((pass + 1))
	printf "  %sPASS%s %s\n" "$c_green" "$c_reset" "$1"
}
bad() {
	fail=$((fail + 1))
	fail_names+=("$1")
	printf "  %sFAIL%s %s%s%s\n" "$c_red" "$c_reset" "$1" "${2:+: $c_dim$2$c_reset}" ""
}
section() { printf "\n%s\n" "==> $1"; }

assert_jq() {
	local name="$1" json="$2" filter="$3" expected="$4"
	local actual
	actual="$(printf '%s' "$json" | jq -r "$filter" 2>&1)" || {
		bad "$name" "jq failed: $actual"
		return
	}
	if [[ "$actual" == *"$expected"* ]]; then
		ok "$name"
	else
		bad "$name" "expected substring '$expected', got '${actual:0:120}'"
	fi
}

assert_jq_nonempty() {
	local name="$1" json="$2" filter="$3"
	local actual
	actual="$(printf '%s' "$json" | jq -r "$filter" 2>&1)" || {
		bad "$name" "jq failed: $actual"
		return
	}
	if [[ -n "$actual" && "$actual" != "null" ]]; then
		ok "$name"
	else
		bad "$name" "expected non-empty for filter '$filter'"
	fi
}

assert_jq_count() {
	local name="$1" json="$2" filter="$3" min="$4"
	local actual
	actual="$(printf '%s' "$json" | jq -r "$filter" 2>&1)" || {
		bad "$name" "jq failed: $actual"
		return
	}
	if [[ "$actual" =~ ^[0-9]+$ && "$actual" -ge "$min" ]]; then
		ok "$name (= $actual)"
	else
		bad "$name" "expected >= $min, got '$actual'"
	fi
}

if ! command -v jq >/dev/null 2>&1; then
	echo "jq is required for the smoke test (sudo dnf install jq / brew install jq)." >&2
	exit 2
fi

# Pick a free port for the HTTP transport test.
free_port() {
	python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null ||
		python -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

# ---------------- Section 1: binary identity ----------------

section "Section 1: binary identity"

version_line="$("$BIN" --version 2>&1)"
if [[ "$version_line" =~ ^legal-mcp\ [0-9]+\.[0-9]+\.[0-9]+ ]]; then
	ok "legal-mcp --version: $version_line"
else
	bad "legal-mcp --version" "got '$version_line'"
fi

help_text="$("$BIN" --help 2>&1)"

# Subcommands that MUST be present.
for cmd in mcp serve activate rollback prune-generations stats verify search fetch \
	get-definition build source-update tree-crawl \
	snapshot-reduce link-download scrape-diff help; do
	if grep -qE "^[[:space:]]+${cmd}([[:space:]]|$)" <<<"$help_text"; then
		ok "subcommand present: $cmd"
	else
		bad "subcommand missing: $cmd"
	fi
done
for removed in update package-corpus publish-release derive-schema11-from-schema10 \
	derive-flat-int8-from-schema11-arroy-v20; do
	if grep -qE "^[[:space:]]+${removed}([[:space:]]|$)" <<<"$help_text"; then
		bad "removed subcommand unexpectedly present: $removed"
	else
		ok "removed subcommand absent: $removed"
	fi
done

# ---------------- Section 2: corpus health ----------------

section "Section 2: corpus health"

stats_json="$("$BIN" stats)"
if printf '%s' "$stats_json" | jq -e . >/dev/null 2>&1; then
	ok "stats returns valid JSON"
else
	bad "stats returned non-JSON" "${stats_json:0:120}"
fi
assert_jq_count "stats.documents > 0" "$stats_json" '.documents' 1
assert_jq_count "stats.chunks > 0" "$stats_json" '.chunks' 1
assert_jq_count "stats.chunk_embeddings > 0" "$stats_json" '.chunk_embeddings' 1
assert_jq_count "stats.definitions > 0" "$stats_json" '.definitions' 1
assert_jq_count "stats ATO prefix_breakdown items" "$stats_json" '.source_stats.ato.prefix_breakdown | length' 5
assert_jq_count "stats ATO documents > 0" "$stats_json" '.source_stats.ato.documents' 1
assert_jq_count "stats FRL documents > 0" "$stats_json" '.source_stats.frl.documents' 1
assert_jq_nonempty "stats.index_version" "$stats_json" '.index_version'
assert_jq_nonempty "stats.active_generation" "$stats_json" '.active_generation'
assert_jq "stats.semantic_search_ready" "$stats_json" '.semantic_search_ready' 'true'
assert_jq "stats.lexical_search_ready" "$stats_json" '.lexical_search_ready' 'true'
if verify_json="$("$BIN" verify)"; then
	assert_jq "verify generation matches stats" "$verify_json" '.active_generation' \
		"$(printf '%s' "$stats_json" | jq -r '.active_generation')"
else
	bad "verify command failed"
fi

# ---------------- Section 3: CLI search ----------------

section "Section 3: CLI search"

# Source is mandatory; omission must fail rather than silently selecting ATO.
if "$BIN" search "section 8-1 deductions" --k 1 >/dev/null 2>&1; then
	bad "search requires source" "omitted source unexpectedly succeeded"
else
	ok "search requires source"
fi
hybrid_json="$("$BIN" search "section 8-1 deductions" --source ato --k 3)"
assert_jq_count "hybrid: hits returned" "$hybrid_json" '.hits | length' 1
assert_jq "hybrid: explicit source is ATO" "$hybrid_json" \
	'(.hits | length > 0) and (.hits | all(.document.source == "ato"))' 'true'
assert_jq_count "hybrid: hits with canonical_url" "$hybrid_json" '[.hits[].canonical_url] | map(select(. != null and . != "")) | length' 1
assert_jq_count "hybrid: hits with typed chunk" "$hybrid_json" '[.hits[].chunk] | map(select(. != null)) | length' 1
assert_jq_count "hybrid: hits with snippet" "$hybrid_json" '[.hits[].snippet] | map(select(. != null and . != "")) | length' 1

# Vector mode.
vector_json="$("$BIN" search "loss carry-back tax offset" --source ato --k 3 --mode vector)"
assert_jq_count "vector mode: hits returned" "$vector_json" '.hits | length' 1

# Keyword mode.
keyword_json="$("$BIN" search "research and development tax incentive" --source ato --k 3 --mode keyword)"
assert_jq_count "keyword mode: hits returned" "$keyword_json" '.hits | length' 1

# Federal Register source routing.
frl_json="$("$BIN" search "income tax assessment act" --source frl --k 3 --mode keyword)"
assert_jq_count "FRL keyword mode: hits returned" "$frl_json" '.hits | length' 1
assert_jq "FRL keyword mode: every hit is FRL" "$frl_json" \
	'(.hits | length > 0) and (.hits | all(.document.source == "frl"))' 'true'

# Type filter.
typed_json="$("$BIN" search "GST going concern" --source ato --k 5 --types TXR)"
assert_jq_count "type filter: hits returned" "$typed_json" '.hits | length' 1
assert_jq "type filter: every hit is TXR" "$typed_json" '[.hits[].type] | unique | join(",")' 'TXR'

# doc_scope filter (glob).
scoped_json="$("$BIN" search "income" --source ato --k 5 --mode keyword --doc-scope 'PAC/%')"
assert_jq_count "doc-scope filter: hits returned" "$scoped_json" '.hits | length' 1
assert_jq "doc-scope filter: every hit under PAC/" "$scoped_json" \
	'(.hits | length > 0) and (.hits | all(.document.native_id | startswith("PAC/")))' 'true'

# Recency sort.
recency_json="$("$BIN" search "small business" --source ato --k 5 --sort-by recency)"
assert_jq_count "recency sort: hits returned" "$recency_json" '.hits | length' 1
assert_jq "recency sort: dates monotonic descending" "$recency_json" \
	'[.hits[].date] | (. == (sort | reverse))' 'true'

# Seed-text fast path (vector-only, no title_hits).
seed_text="A taxpayer who carries on a business may deduct losses or outgoings under section 8-1."
seed_json="$("$BIN" search "ignored when seed-text set" --source ato --k 3 --seed-text "$seed_text")"
assert_jq_count "seed-text: hits returned" "$seed_json" '.hits | length' 1
assert_jq "seed-text: title_hits suppressed" "$seed_json" '.title_hits | length' '0'

# include_old.
current_default="$("$BIN" search "income tax" --source ato --k 3 --mode keyword --types ITR)"
historical="$("$BIN" search "income tax" --source ato --k 3 --mode keyword --types ITR --include-old)"
default_count=$(printf '%s' "$current_default" | jq -r '.hits | length' 2>/dev/null || echo 0)
relaxed_count=$(printf '%s' "$historical" | jq -r '.hits | length' 2>/dev/null || echo 0)
if [[ "$relaxed_count" -ge "$default_count" ]]; then
	ok "include_old: relaxed >= default ($relaxed_count >= $default_count)"
else
	bad "include_old" "relaxed=$relaxed_count default=$default_count"
fi

# Direct native document ID query → exact leading title hit.
direct_json="$("$BIN" search "TXR/TR20007/NAT/ATO/00001" --source ato --k 1)"
assert_jq "direct native ID query: exact title hit leads" "$direct_json" \
	'.title_hits[0].document.native_id' 'TXR/TR20007/NAT/ATO/00001'

# meta.next_call appears when results are truncated.
truncated_json="$("$BIN" search "deductions" --source ato --k 2)"
assert_jq_nonempty "search.meta.next_call present" "$truncated_json" '.meta.next_call'

# ---------------- Section 4: CLI retrieval ----------------

section "Section 4: CLI retrieval"

# get-definition.
defn_json="$("$BIN" get-definition "trading stock" --source ato --max-defs 3)"
assert_jq_count "get-definition: definitions returned" "$defn_json" '.definitions | length' 1
assert_jq "get-definition: statutory_definition_found" "$defn_json" '.statutory_definition_found' 'true'
assert_jq_count "get-definition: body carries [doc:...] markers" "$defn_json" \
	'[.definitions[].body | select(contains("[doc:"))] | length' 1

norm_json="$("$BIN" get-definition "Australian resident" --source ato --max-defs 2)"
if printf '%s' "$norm_json" | jq -e . >/dev/null 2>&1; then
	ok "get-definition normaliser: returns valid JSON on multi-word lowercase term"
else
	bad "get-definition normaliser" "${norm_json:0:120}"
fi

# `fetch` against a known canonical ATO URI (requires network).
fetch_uri='legal://ato/TXR%2FTR20007%2FNAT%2FATO%2F00001'
if [[ "$SKIP_NETWORK" != "1" ]]; then
	fetch_json="$("$BIN" fetch "$fetch_uri")"
	if printf '%s' "$fetch_json" | jq -e . >/dev/null 2>&1; then
		assert_jq_count "fetch: chunks returned" "$fetch_json" '.chunks | length' 1
		assert_jq "fetch: canonical_url" "$fetch_json" '.canonical_url' \
			'docid=TXR%2FTR20007%2FNAT%2FATO%2F00001'
		assert_jq "fetch: canonical URI echoed" "$fetch_json" '.uri' "$fetch_uri"
		assert_jq "fetch: source" "$fetch_json" '.source' 'ato'
		assert_jq "fetch: document identity" "$fetch_json" '.document.native_id' \
			'TXR/TR20007/NAT/ATO/00001'
	else
		bad "fetch" "non-JSON output: ${fetch_json:0:120}"
	fi
else
	printf "  %sSKIP%s fetch (LEGAL_MCP_SKIP_NETWORK=1)\n" "$c_dim" "$c_reset"
fi

# ---------------- Section 5: MCP HTTP transport ----------------

section "Section 5: MCP HTTP transport"

workdir="$(mktemp -d)"
source_data_dir="$(printf '%s' "$stats_json" | jq -r '.data_dir // empty' 2>/dev/null)"
[[ -n "$source_data_dir" ]] || { bad "stats.data_dir missing"; exit 1; }
export LEGAL_MCP_DATA_DIR="$source_data_dir"

port="$(free_port)"
url="http://127.0.0.1:$port/mcp"
log="$workdir/serve.log"
"$BIN" serve --port "$port" >/dev/null 2>"$log" &
serve_pid=$!
trap 'kill '"$serve_pid"' 2>/dev/null; rm -rf "$workdir"' EXIT

# Cold startup verifies all ANN files and prewarms the semantic model.
deadline=$(($(date +%s) + 120))
ready=0
while [[ $(date +%s) -lt $deadline ]]; do
	if grep -q "listening on $url" "$log" 2>/dev/null; then
		ready=1
		break
	fi
	if ! kill -0 "$serve_pid" 2>/dev/null; then
		break
	fi
	sleep 0.1
done
if [[ $ready -eq 1 ]]; then
	ok "server ready on $url"
else
	bad "server failed to start" "log: $(tail -c 500 "$log")"
	echo ""
	echo "Summary: $pass passed, $fail failed"
	exit 1
fi

rpc() {
	local id="$1" method="$2" params="$3"
	local body
	if [[ -n "$params" ]]; then
		body=$(jq -nc --arg m "$method" --argjson p "$params" --arg id "$id" \
			'{jsonrpc:"2.0", id:($id|tonumber), method:$m, params:$p}')
	else
		body=$(jq -nc --arg m "$method" --arg id "$id" \
			'{jsonrpc:"2.0", id:($id|tonumber), method:$m}')
	fi
	curl --silent --show-error --max-time 30 -X POST "$url" \
		-H 'content-type: application/json' \
		-H 'accept: application/json, text/event-stream' \
		-H 'mcp-protocol-version: 2025-06-18' \
		--data-raw "$body"
}

init_resp="$(rpc 1 initialize '{"protocolVersion":"2025-03-26","clientInfo":{"name":"smoke","version":"0"},"capabilities":{}}')"
assert_jq "initialize: server name is australian-legal-mcp" "$init_resp" '.result.serverInfo.name' 'australian-legal-mcp'
assert_jq_nonempty "initialize: instructions present" "$init_resp" '.result.instructions'

tools_resp="$(rpc 2 tools/list '')"
expected_tools=(search get_chunks get_definition get_asset get_doc_anchors fetch stats)
actual_tools="$(printf '%s' "$tools_resp" | jq -r '.result.tools[].name' 2>/dev/null | sort | tr '\n' ' ')"
expected_sorted="$(printf '%s\n' "${expected_tools[@]}" | sort | tr '\n' ' ')"
if [[ "$actual_tools" == "$expected_sorted" ]]; then
	ok "tools/list: exactly the expected tools"
else
	bad "tools/list mismatch" "expected: $expected_sorted | got: $actual_tools"
fi

stats_resp="$(rpc 3 tools/call '{"name":"stats","arguments":{}}')"
assert_jq_nonempty "tools/call stats: response.content" "$stats_resp" '.result.content[0].text'
stats_payload="$(printf '%s' "$stats_resp" | jq -r '.result.content[0].text')"
assert_jq_count "tools/call stats: chunks > 0" "$stats_payload" '.chunks' 1
assert_jq_count "tools/call stats: FRL documents > 0" "$stats_payload" '.source_stats.frl.documents' 1

search_resp="$(rpc 4 tools/call '{"name":"search","arguments":{"query":"capital gains main residence","source":"ato","k":3}}')"
search_payload="$(printf '%s' "$search_resp" | jq -r '.result.content[0].text')"
assert_jq_count "tools/call search: hits returned" "$search_payload" '.hits | length' 1
first_chunk="$(printf '%s' "$search_payload" | jq -c '.hits[0].chunk // empty')"
first_document="$(printf '%s' "$search_payload" | jq -c '.hits[0].document // empty')"
if [[ -n "$first_chunk" ]]; then
	first_chunk_id="$(printf '%s' "$first_chunk" | jq -r '.chunk_id')"
	ok "tools/call search: first typed chunk id=$first_chunk_id"
else
	bad "tools/call search: no typed chunk in first hit"
fi

if [[ -n "$first_chunk" ]]; then
	chunks_args=$(jq -nc --argjson chunk "$first_chunk" \
		'{name:"get_chunks", arguments:{chunks:[$chunk], before:1, after:1}}')
	chunks_resp="$(rpc 5 tools/call "$chunks_args")"
	chunks_payload="$(printf '%s' "$chunks_resp" | jq -r '.result.content[0].text')"
	assert_jq_count "tools/call get_chunks: chunks returned" "$chunks_payload" '.chunks | length' 1
	assert_jq_count "tools/call get_chunks: body text present" "$chunks_payload" \
		'[.chunks[].text] | map(select(. != null and . != "")) | length' 1
fi

defn_resp="$(rpc 6 tools/call '{"name":"get_definition","arguments":{"term":"trading stock","source":"ato","max_defs":2}}')"
defn_payload="$(printf '%s' "$defn_resp" | jq -r '.result.content[0].text')"
assert_jq_count "tools/call get_definition: definitions returned" "$defn_payload" '.definitions | length' 1
assert_jq "tools/call get_definition: statutory hit" "$defn_payload" '.statutory_definition_found' 'true'

if [[ -n "$first_document" ]]; then
	document_label="$(printf '%s' "$first_document" | jq -r '"\(.source):\(.native_id)"')"
	anchors_args=$(jq -nc --argjson document "$first_document" \
		'{name:"get_doc_anchors", arguments:{document:$document}}')
	anchors_resp="$(rpc 7 tools/call "$anchors_args")"
	if printf '%s' "$anchors_resp" | jq -e '.result.content[0].text' >/dev/null 2>&1; then
		ok "tools/call get_doc_anchors: returns content for $document_label"
	else
		bad "tools/call get_doc_anchors" "no content for $document_label"
	fi
fi

asset_resp="$(rpc 8 tools/call '{"name":"get_asset","arguments":{"asset":{"source":"ato","asset_id":"bogus-not-real-doc/9999"}}}')"
if printf '%s' "$asset_resp" | jq -e '.result // .error' >/dev/null 2>&1; then
	ok "tools/call get_asset: structured response for unknown typed asset"
else
	bad "tools/call get_asset" "neither result nor error in response"
fi

if [[ "$SKIP_NETWORK" != "1" ]]; then
	fetch_resp="$(rpc 9 tools/call '{"name":"fetch","arguments":{"uri":"legal://ato/TXR%2FTR20007%2FNAT%2FATO%2F00001"}}')"
	fetch_payload="$(printf '%s' "$fetch_resp" | jq -r '.result.content[0].text' 2>/dev/null)"
	if printf '%s' "$fetch_payload" | jq -e . >/dev/null 2>&1; then
		assert_jq_count "tools/call fetch: chunks returned" "$fetch_payload" '.chunks | length' 1
		assert_jq "tools/call fetch: canonical URI" "$fetch_payload" '.uri' "$fetch_uri"
	else
		bad "tools/call fetch" "non-JSON payload: ${fetch_payload:0:120}"
	fi
else
	printf "  %sSKIP%s tools/call fetch (LEGAL_MCP_SKIP_NETWORK=1)\n" "$c_dim" "$c_reset"
fi

err_resp="$(rpc 10 tools/call '{"name":"definitely_not_a_tool","arguments":{}}')"
if printf '%s' "$err_resp" |
	jq -e '(.error.code != null) or (.result.isError == true)' >/dev/null 2>&1; then
	ok "unknown tool: returns a structured MCP error"
else
	bad "unknown tool" "expected structured error, got: ${err_resp:0:160}"
fi

# ---------------- Section 6: MCP stdio shim ----------------

section "Section 6: MCP stdio shim"

stdio_out="$(printf '%s\n%s\n' \
	'{"jsonrpc":"2.0","id":11,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"smoke","version":"0"},"capabilities":{}}}' \
	'{"jsonrpc":"2.0","id":12,"method":"tools/list"}' |
	"$BIN" mcp)"
stdio_init="$(sed -n '1p' <<<"$stdio_out")"
stdio_tools="$(sed -n '2p' <<<"$stdio_out")"
assert_jq "stdio initialize: server name is australian-legal-mcp" "$stdio_init" '.result.serverInfo.name' 'australian-legal-mcp'
stdio_actual_tools="$(printf '%s' "$stdio_tools" | jq -r '.result.tools[].name' 2>/dev/null | sort | tr '\n' ' ')"
if [[ "$stdio_actual_tools" == "$expected_sorted" ]]; then
	ok "stdio tools/list: exactly the expected tools"
else
	bad "stdio tools/list mismatch" "expected: $expected_sorted | got: $stdio_actual_tools"
fi

# ---------------- summary ----------------

section "Summary"
total=$((pass + fail))
if [[ $fail -eq 0 ]]; then
	printf "%sAll %d tests passed.%s\n" "$c_green" "$total" "$c_reset"
	exit 0
else
	printf "%s%d / %d tests failed:%s\n" "$c_red" "$fail" "$total" "$c_reset"
	for name in "${fail_names[@]}"; do
		printf "  - %s\n" "$name"
	done
	exit 1
fi
