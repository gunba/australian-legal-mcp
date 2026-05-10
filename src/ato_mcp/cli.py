"""``ato-mcp`` CLI entry point.

Maintainer commands:
    ato-mcp refresh-source ...    Scrape (incremental | full) into ato_pages/.
    ato-mcp build-index ...       Produce ato.db + packs + manifest.json.
    ato-mcp release ...           Upload release artifacts.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Optional

import typer

from .util.log import get_logger

LOGGER = get_logger("ato_mcp.cli")

app = typer.Typer(no_args_is_help=True, add_completion=False, help=__doc__)
# [CC-01] Rust owns the installed MCP runtime. This Python CLI is maintainer tooling.
# [CC-06] no_args_is_help=True + add_completion=False — small intentional CLI, no shell-completion magic.


def _maybe_reexec_with_nvidia_libs() -> None:
    if os.environ.get("ATO_MCP_NVIDIA_LIBS_READY"):
        return
    lib_dirs = [
        path
        for path in Path(sys.prefix).glob("lib*/python*/site-packages/nvidia/*/lib")
        if path.is_dir()
    ]
    if not lib_dirs:
        return
    current = [part for part in os.environ.get("LD_LIBRARY_PATH", "").split(":") if part]
    missing = [str(path) for path in lib_dirs if str(path) not in current]
    if not missing:
        os.environ["ATO_MCP_NVIDIA_LIBS_READY"] = "1"
        return
    env = os.environ.copy()
    env["LD_LIBRARY_PATH"] = ":".join([*missing, *current])
    env["ATO_MCP_NVIDIA_LIBS_READY"] = "1"
    os.execvpe(sys.executable, [sys.executable, *sys.argv], env)


@app.command("refresh-source")
def refresh_source(
    mode: str = typer.Option("incremental", help="incremental | full | catch_up | retry_missing"),
    output_dir: Path = typer.Option(Path("./ato_pages"), help="Destination for payloads/."),
    # [CC-05] refresh-source defaults to ./ato_pages; build-index requires --pages-dir pointing at a populated ato_pages/. Stages independently re-runnable — same pages dir can feed multiple builds.
    links_file: Optional[Path] = typer.Option(None, help="deduped_links.jsonl for incremental mode."),
    max_workers: int = typer.Option(1, help="Parallel request workers. Keep low to be polite."),
    request_interval: float = typer.Option(
        0.5,
        help="Minimum seconds between HTTP request starts, globally across workers. "
             "Default 0.5 s = ~2 req/sec. Drop to 1.0 for a slower/safer rate. "
             "retry_missing mode falls back to 0.25 s when this is left at the default.",
    ),
    verbose: bool = typer.Option(False, help="Emit downloader status snapshots."),
    root_query: str = typer.Option(
        "Mode=type&Action=initialise",
        help="Tree root. Override to scope catch_up to a subtree.",
    ),
    max_nodes: Optional[int] = typer.Option(None, help="Cap for debugging."),
    explicit_links_file: Optional[Path] = typer.Option(
        None,
        "--explicit-links-file",
        help=(
            "Optional JSONL file with explicit links to fetch (one record per line: "
            "{\"canonical_id\": \"...\", \"href\": \"...\", \"pit\": \"...\" | null}). "
            "Combined with the standard mode's link source — useful for backfilling "
            "sister docs or PiT historical versions discovered via discover-related-docs.py."
        ),
    ),
) -> None:
    """Maintainer: scrape the ATO site into ``ato_pages/``."""
    from .scraper import refresh_source as run_refresh
    from .util.power import maybe_reexec_with_sleep_inhibitor

    if mode not in {"incremental", "full", "catch_up", "retry_missing"}:
        raise typer.BadParameter(
            f"mode must be one of incremental | full | catch_up | retry_missing (got {mode!r})"
        )

    maybe_reexec_with_sleep_inhibitor(f"ato-mcp source refresh ({mode})")

    explicit_links: Optional[list[dict]] = None
    if explicit_links_file is not None:
        explicit_links = _load_explicit_links_file(explicit_links_file)
        typer.echo(
            f"explicit-links: loaded {len(explicit_links)} record(s) from {explicit_links_file}"
        )

    result = run_refresh(
        mode=mode,  # type: ignore[arg-type]
        output_dir=output_dir,
        links_file=links_file,
        max_workers=max_workers,
        request_interval=request_interval,
        verbose_progress=verbose,
        root_query=root_query,
        max_nodes=max_nodes,
        explicit_links=explicit_links,
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
    if result.retry_missing_summary is not None:
        r = result.retry_missing_summary
        typer.echo(
            f"retry-missing: eligible={r.eligible} recovered={r.recovered} "
            f"confirmed_404={r.confirmed_404} confirmed_stub={r.confirmed_stub} "
            f"still_missing={r.still_missing}"
        )


def _load_explicit_links_file(path: Path) -> list[dict]:
    """Parse a JSONL file of explicit links to fetch.

    Each line must be a JSON object with at minimum ``canonical_id`` and
    ``href`` keys; ``pit`` is optional. Malformed or non-conforming lines are
    skipped with a warning so a single bad row doesn't abort the run.
    """
    import json as _json

    out: list[dict] = []
    if not path.exists():
        raise typer.BadParameter(f"explicit-links file not found: {path}")
    with path.open("r", encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, start=1):
            text = raw.strip()
            if not text:
                continue
            try:
                rec = _json.loads(text)
            except _json.JSONDecodeError as exc:
                LOGGER.warning(
                    "skipping malformed line %d in %s: %s", lineno, path, exc
                )
                continue
            if not isinstance(rec, dict):
                LOGGER.warning(
                    "skipping non-object line %d in %s", lineno, path
                )
                continue
            cid = rec.get("canonical_id")
            href = rec.get("href")
            if not isinstance(cid, str) or not cid:
                LOGGER.warning(
                    "skipping line %d in %s: missing/invalid canonical_id", lineno, path
                )
                continue
            if not isinstance(href, str) or not href:
                LOGGER.warning(
                    "skipping line %d in %s: missing/invalid href", lineno, path
                )
                continue
            out.append(
                {
                    "canonical_id": cid,
                    "href": href,
                    "pit": rec.get("pit"),
                }
            )
    return out


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
    previous_manifest: Optional[Path] = typer.Option(None, help="Previous manifest for incremental reuse."),
    limit: Optional[int] = typer.Option(None, help="Cap documents processed (for testing)."),
    embedder: str = typer.Option(
        "embeddinggemma",
        help="Vectorizer: embeddinggemma.",
    ),
    encode_batch_size: Optional[int] = typer.Option(None, help="Embedding batch size. Default: 64 CPU, 128 GPU."),
    max_batch_tokens: Optional[int] = typer.Option(
        None,
        help="Approx padded tokens allowed in one inference call. Default: 8192 CPU, 12288 GPU.",
    ),
    workers: int = typer.Option(max(1, (os.cpu_count() or 2) - 1), help="HTML extraction workers."),
    window_docs: int = typer.Option(20_000, help="Documents prepared per build window."),
    checkpoint_every: int = typer.Option(
        20_000,
        help="Prepared records per resumable transaction checkpoint.",
    ),
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
    from .util.power import maybe_reexec_with_sleep_inhibitor

    maybe_reexec_with_sleep_inhibitor("ato-mcp corpus rebuild")
    if gpu:
        _maybe_reexec_with_nvidia_libs()

    if embedder != "embeddinggemma":
        raise typer.BadParameter("embedder must be embeddinggemma")
    if model_path is None or tokenizer_path is None:
        raise typer.BadParameter("--model-path and --tokenizer-path are required for embeddinggemma")
    if encode_batch_size is not None and encode_batch_size <= 0:
        raise typer.BadParameter("--encode-batch-size must be positive")
    if max_batch_tokens is not None and max_batch_tokens <= 0:
        raise typer.BadParameter("--max-batch-tokens must be positive")

    model_sha = sha256_file(model_path) if model_path is not None else ""
    model_size = model_path.stat().st_size if model_path is not None else 0
    providers: tuple[str, ...] | None = None
    if gpu:
        providers = ("CUDAExecutionProvider", "CPUExecutionProvider")
    effective_encode_batch_size = encode_batch_size or (128 if gpu else 64)
    effective_max_batch_tokens = max_batch_tokens or (12_288 if gpu else 8_192)
    args = BuildArgs(
        pages_dir=pages_dir,
        out_dir=out_dir,
        db_path=db_path,
        model_id=model_id,
        model_path=model_path,
        tokenizer_path=tokenizer_path,
        model_sha256=model_sha,
        model_size=model_size,
        previous_manifest=previous_manifest,
        limit=limit,
        embedder=embedder,  # type: ignore[arg-type]
        encode_batch_size=effective_encode_batch_size,
        max_batch_tokens=effective_max_batch_tokens,
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
        help="Directory containing onnx/model_quantized.onnx + tokenizer.json (+ optional config.json) "
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


if __name__ == "__main__":
    app()
