#!/usr/bin/env python3
"""Run the existing SDK tests from pinned archives in a loopback-only namespace."""

import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tarfile
import tempfile


def extract(path: Path, digest: str, destination: Path) -> None:
    with path.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if len(digest) != 64 or actual != digest:
        raise ValueError(f"SHA256 mismatch: {path.name}")
    with tarfile.open(path, "r:gz") as archive:
        for member in archive.getmembers():
            if not (member.isfile() or member.isdir()):
                raise ValueError(f"Non-file archive entry: {member.name}")
        archive.extractall(destination, filter="data")


def main() -> None:
    # Refuse a normal network namespace: missing input must never fetch mainnet.
    interfaces = {name for _, name in socket.if_nameindex()}
    if interfaces != {"lo"}:
        raise RuntimeError("Run in a loopback-only network namespace")

    root = Path(tempfile.mkdtemp(prefix="historical-sdk-", dir=os.environ["RUNNER_TEMP"]))
    print(f"Test data/log location: {root}", flush=True)
    extract(
        Path(os.environ["SVM_REPLAY_ARCHIVE"]),
        os.environ["SVM_REPLAY_ARCHIVE_SHA256"],
        root / "runtime",
    )
    extract(
        Path(os.environ["HISTORICAL_FIXTURE_ARCHIVE"]),
        os.environ["HISTORICAL_FIXTURE_SHA256"],
        root / "fixture",
    )
    fixture = root / "fixture" / "svm-replay-smoke-413023260"
    for entry in json.loads((fixture / "files.json").read_text()):
        path = fixture / entry["path"]
        if not path.resolve().is_relative_to(fixture.resolve()):
            raise ValueError("Fixture manifest path escapes its root")
        with path.open("rb") as source:
            actual = hashlib.file_digest(source, "sha256").hexdigest()
        if actual != entry["sha256"]:
            raise ValueError(f"Fixture file mismatch: {entry['path']}")
    metadata = json.loads((fixture / "fixture.json").read_text())
    bundle = root / "runtime" / "bundle"
    with (bundle / "bundle.json").open("rb") as source:
        bundle_digest = hashlib.file_digest(source, "sha256").hexdigest()
    subprocess.run(
        [str(bundle / "bin" / "svm-replay"), "bundle", "install", "--bundle",
         str(bundle / "bundle.json"), "--bundle-sha256", bundle_digest,
         "--output", str(root / "installed")],
        check=True, timeout=120,
    )
    runtime = root / "installed" / "bin" / "svm-replay"
    subprocess.run([str(runtime), "doctor"], check=True, timeout=60)
    env = dict(os.environ)
    env.update(
        API_ALCHEMY="offline-fixture-not-a-key",
        M59_AFTER=metadata["after"],
        M59_SLOT=str(metadata["slot"]),
        M59_SVM_REPLAY_BIN=str(runtime),
        M59_HISTORICAL_DATA_DIR=str(fixture / "data"),
        M59_HISTORICAL_CONFIG=str(fixture / "Surfpool.toml"),
    )
    binary = Path(os.environ["HISTORICAL_TEST_BINARY"]).resolve(strict=True)
    result = subprocess.run(
        [str(binary), "--ignored", "--test-threads=1", "--nocapture"],
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=420,
    )
    (root / "tests.log").write_text(result.stdout)
    print(result.stdout, end="", flush=True)
    result.check_returncode()
    if "3 passed; 0 failed; 0 ignored" not in result.stdout:
        raise RuntimeError("Expected all three existing historical SDK tests to execute")


if __name__ == "__main__":
    main()
