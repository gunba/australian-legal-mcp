"""``ato-mcp`` CLI entry point.

Maintainer commands:
    ato-mcp refresh-source ...    Scrape (incremental | full) into ato_pages/.
    ato-mcp build-index ...       Produce ato.db + packs + manifest.json.
    ato-mcp release ...           Upload release artifacts.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Optional

import typer

from .util import paths
from .util.log import get_logger

LOGGER = get_logger("ato_mcp.cli")

app = typer.Typer(no_args_is_help=True, add_completion=False, help=__doc__)
# [CC-01] Rust owns the installed MCP runtime. This Python CLI is maintainer tooling.
# [CC-06] no_args_is_help=True + add_completion=False — small intentional CLI, no shell-completion magic.


@app.command("refresh-source")
def refresh_source(
    mode: str = typer.Option("incremental", help="incremental | full | catch_up"),
    output_dir: Path = typer.Option(Path("./ato_pages"), help="Destination for payloads/."),
    # [CC-05] refresh-source defaults to ./ato_pages; build-index requires --pages-dir pointing at a populated ato_pages/. Stages independently re-runnable — same pages dir can feed multiple builds.
    links_file: Optional[Path] = typer.Option(None, help="deduped_links.jsonl for incremental mode."),
    max_workers: int = typer.Option(1, help="Parallel request workers. Keep low to be polite."),
    request_interval: float = typer.Option(
        0.5,
        help="Minimum seconds between HTTP request starts, globally across workers. "
             "Default 0.5 s = ~2 req/sec. Drop to 1.0 for a slower/safer rate.",
    ),
    verbose: bool = typer.Option(False, help="Emit downloader status snapshots."),
    root_query: str = typer.Option(
        "Mode=type&Action=initialise",
        help="Tree root. Override to scope catch_up to a subtree.",
    ),
    max_nodes: Optional[int] = typer.Option(None, help="Cap for debugging."),
) -> None:
    """Maintainer: scrape the ATO site into ``ato_pages/``."""
    from .scraper import refresh_source as run_refresh

    result = run_refresh(
        mode=mode,  # type: ignore[arg-type]
        output_dir=output_dir,
        links_file=links_file,
        max_workers=max_workers,
        request_interval=request_interval,
        verbose_progress=verbose,
        root_query=root_query,
        max_nodes=max_nodes,
    )
    typer.echo(f"refresh-source complete: mode={result.mode} output={result.output_dir}")
    if result.catch_up_summary is not None:
        s = result.catch_up_summary
        typer.echo(
            f"catch-up: {s.missing} missing of {s.total_current_links} current "
            f"(existing={s.existing_canonical_ids}); downloaded={s.downloaded}"
        )
        for cat, n in s.by_category.items():
            typer.echo(f"  {n:6d}  {cat}")


@app.command("catch-up")
def catch_up(
    output_dir: Path = typer.Option(..., help="Existing ato_pages/ directory (must contain index.jsonl)."),
    max_workers: int = typer.Option(1, help="Parallel request workers. Keep low to be polite."),
    request_interval: float = typer.Option(
        0.5,
        help="Minimum seconds between HTTP request starts, globally across workers. "
             "Default 0.5 s = ~2 req/sec. Drop to 1.0 for a slower/safer rate.",
    ),
    verbose: bool = typer.Option(False, help="Print downloader status snapshots."),
    root_query: str = typer.Option(
        "Mode=type&Action=initialise",
        help="Tree root. Scope to a subtree for faster runs "
             "(e.g. 'Mode=type&Action=inject&TOC=01%3A%23002%23Public%20rulings').",
    ),
    path_prefix: Optional[str] = typer.Option(
        None,
        help="REQUIRED when --root-query is scoped. Slash-separated ancestor "
             "folders from the absolute root down to the scope, e.g. "
             "'Public_rulings/Rulings/Class'. Omit for a full-tree crawl.",
    ),
    max_nodes: Optional[int] = typer.Option(None, help="Cap nodes crawled (debugging)."),
) -> None:
    """Crawl the ATO tree, diff against the existing index, and download only the
    missing documents. Each new doc is placed into its proper category folder
    automatically via the reducer's representative_path.

    The progress postfix ``crawl_frontier`` is the number of pending browse-tree
    nodes discovered by the crawler. It is not a missing/new document count; that
    count is printed only after the crawl is reduced and diffed.

    Defaults are polite (1 worker, 1.0 s between requests = ~1 req/sec).
    A full-tree catch-up takes hours at these rates — scope with
    ``--root-query`` + ``--path-prefix`` when you only need recent docs.
    This command is for missing canonical documents, not for retrying
    empty-shell documents that the ATO served without body content.

    Full catch-up:
        ato-mcp catch-up --output-dir ./ato_pages

    Scoped catch-up — must supply path_prefix so paths line up:
        ato-mcp catch-up --output-dir ./ato_pages \\
          --root-query 'Mode=type&Action=inject&TOC=03%3APublic%20rulings%3ARulings%3A%23011%23Class' \\
          --path-prefix 'Public_rulings/Rulings/Class'
    """
    from .scraper import refresh_source as run_refresh

    prefix = [p for p in (path_prefix or "").split("/") if p] or None
    result = run_refresh(
        mode="catch_up",
        output_dir=output_dir,
        max_workers=max_workers,
        request_interval=request_interval,
        verbose_progress=verbose,
        root_query=root_query,
        max_nodes=max_nodes,
        path_prefix=prefix,
    )
    s = result.catch_up_summary
    assert s is not None
    typer.echo(
        f"catch-up: {s.missing} missing of {s.total_current_links} current "
        f"(existing={s.existing_canonical_ids}); downloaded={s.downloaded}"
    )
    for cat, n in s.by_category.items():
        typer.echo(f"  {n:6d}  {cat}")
    typer.echo(f"diff_file: {s.diff_file}")


@app.command("build-index")
def build_index(
    pages_dir: Path = typer.Option(..., help="Directory produced by refresh-source (contains index.jsonl)."),
    out_dir: Path = typer.Option(Path("./release"), help="Where to write manifest.json + packs/."),
    db_path: Path = typer.Option(Path("./release/ato.db")),
    model_path: Optional[Path] = typer.Option(None, help="Path to embeddinggemma ONNX file."),
    tokenizer_path: Optional[Path] = typer.Option(None, help="Path to tokenizer.json."),
    model_id: str = typer.Option("embeddinggemma-300m-int8-256d"),
    model_url: Optional[str] = typer.Option(None),
    reranker_id: Optional[str] = typer.Option(
        None,
        help="Optional reranker model id; combined with --reranker-url to populate "
             "the manifest's `reranker` ModelInfo. Leave unset to publish a manifest "
             "without a reranker entry (Rust runtime falls back to un-reranked).",
    ),
    reranker_url: Optional[str] = typer.Option(
        None,
        help="Optional reranker source URL (e.g. hf://Alibaba-NLP/gte-reranker-modernbert-base@<sha>).",
    ),
    reranker_sha256: Optional[str] = typer.Option(
        None, help="sha256 of the externally hosted reranker ONNX file."
    ),
    reranker_size: Optional[int] = typer.Option(
        None, help="Size in bytes of the externally hosted reranker ONNX file."
    ),
    reranker_tokenizer_sha256: Optional[str] = typer.Option(
        None,
        help="Optional sha256 of the externally hosted reranker tokenizer.json. "
             "When provided, the Rust runtime verifies the downloaded tokenizer "
             "byte-for-byte; otherwise it logs a one-line warning and skips.",
    ),
    previous_manifest: Optional[Path] = typer.Option(None, help="Previous manifest for incremental reuse."),
    limit: Optional[int] = typer.Option(None, help="Cap documents processed (for testing)."),
    embedder: str = typer.Option(
        "embeddinggemma",
        help="Vectorizer: embeddinggemma.",
    ),
    encode_batch_size: int = typer.Option(64, help="Embedding batch size. Bump for GPU."),
    max_batch_tokens: int = typer.Option(8192, help="Approx padded tokens allowed in one inference call."),
    workers: int = typer.Option(max(1, (os.cpu_count() or 2) - 1), help="HTML extraction workers."),
    window_docs: int = typer.Option(20_000, help="Documents prepared per build window."),
    checkpoint_every: int = typer.Option(1_000_000_000, help="Prepared records per transaction checkpoint."),
    unsafe_fast_sqlite: bool = typer.Option(
        False,
        help="Use scratch-build SQLite pragmas that favor speed over crash recovery.",
    ),
    zstd_level: int = typer.Option(3, help="zstd level for chunk and pack compression."),
    pack_target_mb: int = typer.Option(64, help="Approx uncompressed payload per pack."),
    gpu: bool = typer.Option(False, "--gpu/--cpu", help="Use CUDAExecutionProvider when available."),
) -> None:
    """Maintainer: build a fresh index + packs + manifest from ``ato_pages/``."""
    from .indexer.build import BuildArgs
    from .indexer.build import build as run_build
    from .store.manifest import sha256_file

    if embedder != "embeddinggemma":
        raise typer.BadParameter("embedder must be embeddinggemma")
    if model_path is None or tokenizer_path is None:
        raise typer.BadParameter("--model-path and --tokenizer-path are required for embeddinggemma")

    model_sha = sha256_file(model_path) if model_path is not None else ""
    model_size = model_path.stat().st_size if model_path is not None else 0
    providers: tuple[str, ...] | None = None
    if gpu:
        providers = ("CUDAExecutionProvider", "CPUExecutionProvider")
    args = BuildArgs(
        pages_dir=pages_dir,
        out_dir=out_dir,
        db_path=db_path,
        model_id=model_id,
        model_path=model_path,
        tokenizer_path=tokenizer_path,
        model_url=model_url,
        model_sha256=model_sha,
        model_size=model_size,
        reranker_id=reranker_id,
        reranker_url=reranker_url,
        reranker_sha256=reranker_sha256,
        reranker_size=reranker_size,
        reranker_tokenizer_sha256=reranker_tokenizer_sha256,
        previous_manifest=previous_manifest,
        limit=limit,
        embedder=embedder,  # type: ignore[arg-type]
        encode_batch_size=encode_batch_size,
        max_batch_tokens=max_batch_tokens,
        providers=providers,
        workers=workers,
        window_docs=window_docs,
        checkpoint_every=checkpoint_every,
        unsafe_fast_sqlite=unsafe_fast_sqlite,
        zstd_level=zstd_level,
        pack_target_size=pack_target_mb * 1024 * 1024,
    )
    manifest = run_build(args)
    typer.echo(
        f"build-index complete: {len(manifest.documents)} docs, {len(manifest.packs)} packs"
    )


@app.command()
def release(
    out_dir: Path = typer.Option(..., help="Directory produced by build-index."),
    tag: str = typer.Option(..., help="GitHub release tag, e.g. index-2026.04.18."),
    repo: Optional[str] = typer.Option(None, help="owner/repo; defaults to gh's default."),
    title: Optional[str] = typer.Option(None),
    notes: Optional[str] = typer.Option(None, help="Release notes body; omit for auto-generated."),
    draft: bool = typer.Option(False, help="Create as a draft release."),
    prerelease: bool = typer.Option(False, help="Mark as prerelease."),
    sign_key: Optional[Path] = typer.Option(None, help="minisign secret-key file for signing the manifest."),
    overwrite: bool = typer.Option(False, help="Replace existing assets on the release (gh release upload --clobber)."),
    model_dir: Optional[Path] = typer.Option(
        None, help="Directory holding the embedding ONNX + tokenizer; creates a local bundle for external hosting."
    ),
    model_url: Optional[str] = typer.Option(
        None, help="Approved model mirror URL; defaults to pinned Hugging Face EmbeddingGemma files."
    ),
    model_sha256: Optional[str] = typer.Option(None, help="sha256 of an externally hosted model bundle."),
    model_size: Optional[int] = typer.Option(None, help="Size in bytes of an externally hosted model bundle."),
    reranker_bundle: Optional[Path] = typer.Option(
        None,
        help="Directory containing model_quantized.onnx + tokenizer.json (+ optional config.json) "
             "for the cross-encoder reranker. The bundle is NOT uploaded to GitHub; only its "
             "sha256/size are recorded in the manifest's `reranker` ModelInfo. The Rust runtime "
             "fetches the actual ONNX from --reranker-url (default: pinned Hugging Face revision).",
    ),
    reranker_id: Optional[str] = typer.Option(
        None,
        help="Reranker model id stored in the manifest (default: gte-reranker-modernbert-base-quantized).",
    ),
    reranker_url: Optional[str] = typer.Option(
        None,
        help="External (HF) URL the Rust runtime fetches the reranker ONNX from.",
    ),
    reranker_sha256: Optional[str] = typer.Option(
        None, help="Override the bundle's computed sha256 (rare — use when hosting a re-packed bundle)."
    ),
    reranker_size: Optional[int] = typer.Option(
        None, help="Override the bundle's computed size (rare — see --reranker-sha256)."
    ),
    reranker_tokenizer_sha256: Optional[str] = typer.Option(
        None,
        help="Override the auto-derived tokenizer.json sha256 (rare — set when "
             "publishing a manifest pointing at an HF revision whose tokenizer "
             "you've vetted out-of-band).",
    ),
) -> None:
    """Maintainer: upload the build artifacts to a GitHub release.

    Shells out to the local ``gh`` CLI; use ``gh auth login`` beforehand.
    """
    from .indexer.release import ReleaseArgs, publish

    publish(ReleaseArgs(
        out_dir=out_dir,
        tag=tag,
        repo=repo,
        title=title,
        notes=notes,
        draft=draft,
        prerelease=prerelease,
        sign_key=sign_key,
        overwrite=overwrite,
        model_dir=model_dir,
        model_url=model_url,
        model_sha256=model_sha256,
        model_size=model_size,
        reranker_bundle=reranker_bundle,
        reranker_id=reranker_id,
        reranker_url=reranker_url,
        reranker_sha256=reranker_sha256,
        reranker_size=reranker_size,
        reranker_tokenizer_sha256=reranker_tokenizer_sha256,
    ))
    typer.echo(f"release {tag} published with manifest + packs")


# ---------------------------------------------------------------------------
# Empty-shell inspection.


shells_app = typer.Typer(
    no_args_is_help=True,
    help="Inspect the empty_shells log (docs the scraper fetched but which "
         "yielded no extractable content).",
)
app.add_typer(shells_app, name="empty-shells")


@shells_app.command("count")
def shells_count(
    db_path: Optional[Path] = typer.Option(
        None, help="Path to ato.db. Defaults to the live install path."
    ),
) -> None:
    """Print total + top-20 doc_id-prefix breakdown."""
    from .store import db as store_db
    target = db_path or paths.db_path()
    conn = store_db.connect(target, mode="ro")
    try:
        total = conn.execute("SELECT COUNT(*) AS n FROM empty_shells").fetchone()["n"]
        typer.echo(f"total shells: {total:,}")
        rows = conn.execute(
            """
            SELECT substr(doc_id, 1, instr(doc_id||'/', '/')-1) AS prefix,
                   COUNT(*) AS n
            FROM empty_shells GROUP BY prefix ORDER BY n DESC LIMIT 20
            """
        ).fetchall()
        for r in rows:
            typer.echo(f"  {r['prefix']:<8} {r['n']:>7,}")
    finally:
        conn.close()


@shells_app.command("list")
def shells_list(
    limit: int = typer.Option(20, help="How many shells to show."),
    prefix: Optional[str] = typer.Option(
        None, help="Filter by doc_id prefix (e.g. 'EV', 'JUD')."
    ),
    db_path: Optional[Path] = typer.Option(None),
) -> None:
    """Show the N oldest-unchecked shells for diagnostics."""
    from .store import db as store_db
    target = db_path or paths.db_path()
    conn = store_db.connect(target, mode="ro")
    try:
        sql = (
            "SELECT doc_id, first_seen_at, last_checked_at, check_count, source "
            "FROM empty_shells"
        )
        params: list = []
        if prefix:
            sql += " WHERE doc_id LIKE ?"
            params.append(f"{prefix.rstrip('/')}/%")
        sql += " ORDER BY last_checked_at ASC LIMIT ?"
        params.append(limit)
        rows = conn.execute(sql, params).fetchall()
        for r in rows:
            typer.echo(
                f"{r['doc_id']:<50}  first={r['first_seen_at']}  "
                f"last={r['last_checked_at']}  n={r['check_count']}  "
                f"src={r['source'] or ''}"
            )
    finally:
        conn.close()


@shells_app.command("export")
def shells_export(
    out: Path = typer.Argument(..., help="Destination CSV path."),
    db_path: Optional[Path] = typer.Option(None),
) -> None:
    """Dump the whole table to CSV (doc_id, canonical_url, first_seen_at, last_checked_at, check_count, source)."""
    import csv
    from .store import db as store_db
    target = db_path or paths.db_path()
    conn = store_db.connect(target, mode="ro")
    try:
        rows = conn.execute(
            "SELECT doc_id, first_seen_at, last_checked_at, check_count, source "
            "FROM empty_shells ORDER BY doc_id"
        ).fetchall()
        with out.open("w", newline="", encoding="utf-8") as fh:
            w = csv.writer(fh)
            w.writerow(["doc_id", "canonical_url", "first_seen_at",
                        "last_checked_at", "check_count", "source"])
            for r in rows:
                w.writerow([
                    r["doc_id"], f"https://www.ato.gov.au/law/view/document?docid={r['doc_id']}",
                    r["first_seen_at"], r["last_checked_at"],
                    r["check_count"], r["source"] or "",
                ])
        typer.echo(f"wrote {len(rows):,} rows to {out}")
    finally:
        conn.close()


if __name__ == "__main__":
    app()
