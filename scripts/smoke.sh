#!/usr/bin/env bash
# End-to-end smoke test for an installed ato-mcp binary + corpus.
#
# Verifies:
#   1. Binary identity (version, expected subcommand surface, no dead commands)
#   2. Corpus health via `stats`
#   3. CLI search (hybrid/vector/keyword modes, type/date/scope filters, recency
#      sort, seed_text fast path, direct doc_id title hit, include_old)
#   4. CLI retrieval (get-definition, fetch)
#   5. MCP HTTP transport (serve, initialize, tools/list, all tools)
#
# Read-only against the live corpus; the HTTP transport tests use a tempdir
# so they do not collide with the user's running server.
#
# Usage:
#   scripts/smoke.sh
#   ATO_MCP_BIN=/path/to/ato-mcp scripts/smoke.sh
#   ATO_MCP_SKIP_NETWORK=1 scripts/smoke.sh   # skip `fetch` (network)
set -uo pipefail

BIN="${ATO_MCP_BIN:-$HOME/.local/bin/ato-mcp}"
SKIP_NETWORK="${ATO_MCP_SKIP_NETWORK:-0}"

if [[ ! -x "$BIN" ]]; then
    echo "ato-mcp binary not found or not executable: $BIN" >&2
    echo "Set ATO_MCP_BIN or put the binary at \$HOME/.local/bin/ato-mcp." >&2
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
    c_green=""; c_red=""; c_dim=""; c_reset=""
fi

ok()   { pass=$((pass+1)); printf "  %sPASS%s %s\n" "$c_green" "$c_reset" "$1"; }
bad()  { fail=$((fail+1)); fail_names+=("$1"); printf "  %sFAIL%s %s%s%s\n" "$c_red" "$c_reset" "$1" "${2:+: $c_dim$2$c_reset}" ""; }
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
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' 2>/dev/null || \
    python -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

# ---------------- Section 1: binary identity ----------------

section "Section 1: binary identity"

version_line="$("$BIN" --version 2>&1)"
if [[ "$version_line" =~ ^ato-mcp\ [0-9]+\.[0-9]+\.[0-9]+ ]]; then
    ok "ato-mcp --version: $version_line"
else
    bad "ato-mcp --version" "got '$version_line'"
fi

help_text="$("$BIN" --help 2>&1)"

# Subcommands that MUST be present.
for cmd in serve update stats search fetch austlii \
           get-definition build tree-crawl snapshot-reduce link-download scrape-diff \
           bundle-localize-manifest publish-release help; do
    if grep -qE "^[[:space:]]+${cmd}[[:space:]]" <<<"$help_text"; then
        ok "subcommand present: $cmd"
    else
        bad "subcommand missing: $cmd"
    fi
done

# Subcommands that MUST NOT appear (removed during the cleanup).
for cmd in daemon install-http doctor search-austlii extract extract-definitions extract-anchors \
           extract-currency chunk-html doc-meta doc-id-from-link pack-write \
           manifest-rewrite-urls bundle-model ato-fetch-nodes embed whats-new \
           normalize-doc-href check-build-checkpoint; do
    if grep -qE "^[[:space:]]+${cmd}[[:space:]]" <<<"$help_text"; then
        bad "dead subcommand still listed: $cmd"
    else
        ok "removed subcommand absent: $cmd"
    fi
done

# ---------------- Section 2: corpus health ----------------

section "Section 2: corpus health"

stats_json="$("$BIN" stats 2>&1)"
if printf '%s' "$stats_json" | jq -e . >/dev/null 2>&1; then
    ok "stats returns valid JSON"
else
    bad "stats returned non-JSON" "${stats_json:0:120}"
fi
assert_jq_count "stats.documents > 0"        "$stats_json" '.documents'        1
assert_jq_count "stats.chunks > 0"           "$stats_json" '.chunks'           1
assert_jq_count "stats.chunk_embeddings > 0" "$stats_json" '.chunk_embeddings' 1
assert_jq_count "stats.definitions > 0"      "$stats_json" '.definitions'      1
assert_jq_count "stats.prefix_breakdown items" "$stats_json" '.prefix_breakdown | length' 5
assert_jq_nonempty "stats.embedding_model_id" "$stats_json" '.embedding_model_id'
assert_jq_nonempty "stats.index_version"      "$stats_json" '.index_version'
assert_jq "stats.semantic_search_ready"      "$stats_json" '.semantic_search_ready' 'true'
assert_jq_nonempty "stats.austlii block present" "$stats_json" '.austlii'

# ---------------- Section 3: CLI search ----------------

section "Section 3: CLI search"

# Hybrid (default).
hybrid_json="$("$BIN" search "section 8-1 deductions" --k 3 2>&1)"
assert_jq_count   "hybrid: hits returned" "$hybrid_json" '.hits | length' 1
assert_jq_nonempty "hybrid: each hit has canonical_url" "$hybrid_json" '[.hits[].canonical_url] | map(select(. != null and . != "")) | length'
assert_jq_nonempty "hybrid: each hit has chunk_id"      "$hybrid_json" '[.hits[].chunk_id]      | map(select(. != null)) | length'
assert_jq_nonempty "hybrid: each hit has snippet"       "$hybrid_json" '[.hits[].snippet]       | map(select(. != null and . != "")) | length'

# Vector mode.
vector_json="$("$BIN" search "loss carry-back tax offset" --k 3 --mode vector 2>&1)"
assert_jq_count "vector mode: hits returned" "$vector_json" '.hits | length' 1

# Keyword mode.
keyword_json="$("$BIN" search "research and development tax incentive" --k 3 --mode keyword 2>&1)"
assert_jq_count "keyword mode: hits returned" "$keyword_json" '.hits | length' 1

# Type filter.
typed_json="$("$BIN" search "GST going concern" --k 5 --types TXR 2>&1)"
assert_jq_count "type filter: hits returned" "$typed_json" '.hits | length' 1
assert_jq      "type filter: every hit is TXR" "$typed_json" '[.hits[].type] | unique | join(",")' 'TXR'

# doc_scope filter (glob).
scoped_json="$("$BIN" search "income" --k 5 --doc-scope 'PAC/%' 2>&1)"
assert_jq_count "doc-scope filter: hits returned" "$scoped_json" '.hits | length' 1
assert_jq      "doc-scope filter: every hit under PAC/" "$scoped_json" \
    '(.hits | length > 0) and (.hits | all(.doc_id | startswith("PAC/")))' 'true'

# Recency sort.
recency_json="$("$BIN" search "small business" --k 5 --sort-by recency 2>&1)"
assert_jq_count "recency sort: hits returned" "$recency_json" '.hits | length' 1
assert_jq      "recency sort: dates monotonic descending" "$recency_json" \
    '[.hits[].date] | (. == (sort | reverse))' 'true'

# Seed-text fast path (vector-only, no title_hits).
seed_text="A taxpayer who carries on a business may deduct losses or outgoings under section 8-1."
seed_json="$("$BIN" search "ignored when seed-text set" --k 3 --seed-text "$seed_text" 2>&1)"
assert_jq_count "seed-text: hits returned" "$seed_json" '.hits | length' 1
assert_jq      "seed-text: title_hits suppressed" "$seed_json" '.title_hits | length' '0'

# include_old.
old_default="$("$BIN" search "fringe benefits" --k 3 --types FBR 2>&1)"
old_relaxed="$("$BIN" search "fringe benefits" --k 3 --types FBR --include-old 2>&1)"
default_count=$(printf '%s' "$old_default" | jq -r '.hits | length' 2>/dev/null || echo 0)
relaxed_count=$(printf '%s' "$old_relaxed" | jq -r '.hits | length' 2>/dev/null || echo 0)
if [[ "$relaxed_count" -ge "$default_count" ]]; then
    ok "include_old: relaxed >= default ($relaxed_count >= $default_count)"
else
    bad "include_old" "relaxed=$relaxed_count default=$default_count"
fi

# Direct doc_id query → title hit.
direct_json="$("$BIN" search "TXR/TR20007/NAT/ATO/00001" --k 1 2>&1)"
assert_jq "direct doc_id query: title_hit present" "$direct_json" \
    '.title_hits[0].doc_id' 'TXR/TR20007/NAT/ATO/00001'

# meta.next_call appears when results are truncated.
truncated_json="$("$BIN" search "deductions" --k 2 2>&1)"
assert_jq_nonempty "search.meta.next_call present" "$truncated_json" '.meta.next_call'

# ---------------- Section 4: CLI retrieval ----------------

section "Section 4: CLI retrieval"

# get-definition.
defn_json="$("$BIN" get-definition "trading stock" --max-defs 3 2>&1)"
assert_jq_count "get-definition: definitions returned" "$defn_json" '.definitions | length' 1
assert_jq      "get-definition: statutory_definition_found"  "$defn_json" '.statutory_definition_found' 'true'
assert_jq_nonempty "get-definition: body carries [doc:...] markers" "$defn_json" \
    '[.definitions[].body | select(contains("[doc:"))] | length'

norm_json="$("$BIN" get-definition "Australian resident" --max-defs 2 2>&1)"
if printf '%s' "$norm_json" | jq -e . >/dev/null 2>&1; then
    ok "get-definition normaliser: returns valid JSON on multi-word lowercase term"
else
    bad "get-definition normaliser" "${norm_json:0:120}"
fi

# `fetch` against a known ATO doc_id (requires network).
if [[ "$SKIP_NETWORK" != "1" ]]; then
    fetch_json="$("$BIN" fetch ato:TXR/TR20007/NAT/ATO/00001 2>&1)"
    if printf '%s' "$fetch_json" | jq -e . >/dev/null 2>&1; then
        assert_jq_count   "fetch: chunks returned" "$fetch_json" '.chunks | length' 1
        assert_jq         "fetch: canonical_url"   "$fetch_json" '.canonical_url' \
            'docid=TXR/TR20007/NAT/ATO/00001'
        assert_jq         "fetch: uri echoed"      "$fetch_json" '.uri' \
            'ato:TXR/TR20007/NAT/ATO/00001'
        assert_jq         "fetch: source flag"     "$fetch_json" '.source' 'live'
    else
        bad "fetch" "non-JSON output: ${fetch_json:0:120}"
    fi
else
    printf "  %sSKIP%s fetch (ATO_MCP_SKIP_NETWORK=1)\n" "$c_dim" "$c_reset"
fi

# ---------------- Section 5: MCP HTTP transport ----------------

section "Section 5: MCP HTTP transport"

workdir="$(mktemp -d)"
export ATO_MCP_DATA_DIR="$workdir/data"

# Share the live corpus by symlinking the user's live dir into the tempdir.
live_src="$HOME/.local/share/ato-mcp/live"
mkdir -p "$ATO_MCP_DATA_DIR"
if [[ -d "$live_src" ]]; then
    ln -s "$live_src" "$ATO_MCP_DATA_DIR/live"
    if [[ -f "$HOME/.local/share/ato-mcp/installed_manifest.json" ]]; then
        ln -sf "$HOME/.local/share/ato-mcp/installed_manifest.json" \
            "$ATO_MCP_DATA_DIR/installed_manifest.json"
    fi
    ok "linked live corpus into tempdir"
else
    bad "live corpus not found at $live_src — install one with: ato-mcp update"
    rm -rf "$workdir"
    echo ""
    echo "Summary: $pass passed, $fail failed"
    exit 1
fi

port="$(free_port)"
url="http://127.0.0.1:$port/mcp"
log="$workdir/serve.log"
"$BIN" serve --port "$port" >/dev/null 2>"$log" &
serve_pid=$!
trap 'kill '"$serve_pid"' 2>/dev/null; rm -rf "$workdir"' EXIT

# Wait for the readiness line — bounded to ~10 s.
deadline=$(( $(date +%s) + 10 ))
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
        --data-raw "$body"
}

init_resp="$(rpc 1 initialize '{"protocolVersion":"2025-03-26","clientInfo":{"name":"smoke","version":"0"},"capabilities":{}}')"
assert_jq "initialize: server name is ato-mcp" "$init_resp" '.result.serverInfo.name' 'ato-mcp'
assert_jq_nonempty "initialize: instructions present"   "$init_resp" '.result.instructions'

tools_resp="$(rpc 2 tools/list '')"
expected_tools=(search get_chunks get_definition get_asset get_doc_anchors fetch search_austlii stats)
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

search_resp="$(rpc 4 tools/call '{"name":"search","arguments":{"query":"capital gains main residence","k":3}}')"
search_payload="$(printf '%s' "$search_resp" | jq -r '.result.content[0].text')"
assert_jq_count "tools/call search: hits returned" "$search_payload" '.hits | length' 1
first_chunk_id="$(printf '%s' "$search_payload" | jq -r '.hits[0].chunk_id // empty')"
first_doc_id="$(printf '%s'   "$search_payload" | jq -r '.hits[0].doc_id   // empty')"
if [[ -n "$first_chunk_id" ]]; then
    ok "tools/call search: first hit chunk_id=$first_chunk_id"
else
    bad "tools/call search: no chunk_id in first hit"
fi

if [[ -n "$first_chunk_id" ]]; then
    chunks_args=$(jq -nc --argjson cid "$first_chunk_id" '{name:"get_chunks", arguments:{chunk_ids:[$cid], before:1, after:1}}')
    chunks_resp="$(rpc 5 tools/call "$chunks_args")"
    chunks_payload="$(printf '%s' "$chunks_resp" | jq -r '.result.content[0].text')"
    assert_jq_count "tools/call get_chunks: chunks returned" "$chunks_payload" '.chunks | length' 1
    assert_jq_nonempty "tools/call get_chunks: body text present" "$chunks_payload" \
        '[.chunks[].text] | map(select(. != null and . != "")) | length'
fi

defn_resp="$(rpc 6 tools/call '{"name":"get_definition","arguments":{"term":"trading stock","max_defs":2}}')"
defn_payload="$(printf '%s' "$defn_resp" | jq -r '.result.content[0].text')"
assert_jq_count "tools/call get_definition: definitions returned" "$defn_payload" '.definitions | length' 1
assert_jq      "tools/call get_definition: statutory hit"          "$defn_payload" '.statutory_definition_found' 'true'

if [[ -n "$first_doc_id" ]]; then
    anchors_args=$(jq -nc --arg did "$first_doc_id" '{name:"get_doc_anchors", arguments:{doc_id:$did}}')
    anchors_resp="$(rpc 7 tools/call "$anchors_args")"
    if printf '%s' "$anchors_resp" | jq -e '.result.content[0].text' >/dev/null 2>&1; then
        ok "tools/call get_doc_anchors: returns content for $first_doc_id"
    else
        bad "tools/call get_doc_anchors" "no content for $first_doc_id"
    fi
fi

asset_resp="$(rpc 8 tools/call '{"name":"get_asset","arguments":{"asset_ref":"asset:bogus-not-real-doc/9999"}}')"
if printf '%s' "$asset_resp" | jq -e '.result // .error' >/dev/null 2>&1; then
    ok "tools/call get_asset: structured response for unknown ref"
else
    bad "tools/call get_asset" "neither result nor error in response"
fi

if [[ "$SKIP_NETWORK" != "1" ]]; then
    fetch_resp="$(rpc 9 tools/call '{"name":"fetch","arguments":{"uri":"ato:TXR/TR20007/NAT/ATO/00001"}}')"
    fetch_payload="$(printf '%s' "$fetch_resp" | jq -r '.result.content[0].text' 2>/dev/null)"
    if printf '%s' "$fetch_payload" | jq -e . >/dev/null 2>&1; then
        assert_jq_count "tools/call fetch: chunks returned" "$fetch_payload" '.chunks | length' 1
    else
        bad "tools/call fetch" "non-JSON payload: ${fetch_payload:0:120}"
    fi
else
    printf "  %sSKIP%s tools/call fetch (ATO_MCP_SKIP_NETWORK=1)\n" "$c_dim" "$c_reset"
fi

err_resp="$(rpc 10 tools/call '{"name":"definitely_not_a_tool","arguments":{}}')"
if printf '%s' "$err_resp" | jq -e '.error.code' >/dev/null 2>&1; then
    ok "unknown tool: returns JSON-RPC error"
else
    bad "unknown tool" "expected error envelope, got: ${err_resp:0:160}"
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
