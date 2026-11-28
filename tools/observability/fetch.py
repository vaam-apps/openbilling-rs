#!/usr/bin/env python3
"""Fetches pinned binaries without a container runtime. Standard library only.

  fetch.py image REPO DIGEST DEST PATH[:NAME]...   extract files from a Docker Hub image (linux/amd64)
  fetch.py url URL SHA256 DEST                     download a file and check its sha256

`image` reads the registry's v2 API: the index named by DIGEST (its sha256 is checked), the linux/amd64 manifest, then
each layer (its sha256 is checked), and extracts only the named files (tarfile filter "data"). Nothing is trusted by
tag. A file already present in DEST is kept (the cache is keyed by the pins in versions.env).
"""

import hashlib
import json
import os
import shutil
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

REGISTRY = "https://registry-1.docker.io"
ACCEPT = ",".join(
    [
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    ]
)


def get(url, headers=None):
    """GET with a bounded retry: registries answer 429 and 5xx under load."""
    request = urllib.request.Request(url, headers=headers or {})
    for attempt in range(6):
        try:
            return urllib.request.urlopen(request, timeout=300)
        except urllib.error.HTTPError as error:
            if error.code not in (429, 500, 502, 503, 504) or attempt == 5:
                raise
            time.sleep(2 ** (attempt + 1))


def token(repo):
    url = f"https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repo}:pull"
    with get(url) as response:
        return json.load(response)["token"]


def manifest(repo, tok, digest):
    with get(f"{REGISTRY}/v2/{repo}/manifests/{digest}", {"Authorization": f"Bearer {tok}", "Accept": ACCEPT}) as r:
        data = r.read()
    if "sha256:" + hashlib.sha256(data).hexdigest() != digest:
        sys.exit(f"digest mismatch for manifest {digest}")
    return json.loads(data)


def image(repo, digest, dest, wanted):
    dest = os.path.abspath(dest)
    os.makedirs(dest, exist_ok=True)
    targets = {}
    for item in wanted:
        path, _, name = item.partition(":")
        targets[path.lstrip("/")] = name or os.path.basename(path)
    if all(os.path.exists(os.path.join(dest, name)) for name in targets.values()):
        return
    tok = token(repo)
    document = manifest(repo, tok, digest)
    if "manifests" in document:
        chosen = [
            m
            for m in document["manifests"]
            if m.get("platform", {}).get("os") == "linux" and m.get("platform", {}).get("architecture") == "amd64"
        ]
        if len(chosen) != 1:
            sys.exit("no single linux/amd64 manifest")
        document = manifest(repo, tok, chosen[0]["digest"])
    found = set()
    for layer in document["layers"]:
        with tempfile.TemporaryFile() as handle:
            hasher = hashlib.sha256()
            with get(f"{REGISTRY}/v2/{repo}/blobs/{layer['digest']}", {"Authorization": f"Bearer {tok}"}) as response:
                while chunk := response.read(1 << 20):
                    hasher.update(chunk)
                    handle.write(chunk)
            if "sha256:" + hasher.hexdigest() != layer["digest"]:
                sys.exit(f"layer digest mismatch {layer['digest']}")
            handle.seek(0)
            with tarfile.open(fileobj=handle, mode="r:*") as archive:
                for member in archive:
                    key = member.name[2:] if member.name.startswith("./") else member.name
                    if key in targets and member.isfile():
                        with tempfile.TemporaryDirectory() as scratch:
                            archive.extract(member, scratch, filter="data")
                            out = os.path.join(dest, targets[key])
                            shutil.move(os.path.join(scratch, key), out)
                            os.chmod(out, 0o755)
                        found.add(key)
    missing = set(targets) - found
    if missing:
        sys.exit(f"not in the image: {sorted(missing)}")


def url(source, sha256, dest):
    if os.path.exists(dest):
        hasher = hashlib.sha256()
        with open(dest, "rb") as handle:
            while chunk := handle.read(1 << 20):
                hasher.update(chunk)
        if hasher.hexdigest() == sha256:
            return
    os.makedirs(os.path.dirname(os.path.abspath(dest)), exist_ok=True)
    part = dest + ".part"
    hasher = hashlib.sha256()
    with get(source) as response, open(part, "wb") as handle:
        while chunk := response.read(1 << 20):
            hasher.update(chunk)
            handle.write(chunk)
    if hasher.hexdigest() != sha256:
        os.remove(part)
        sys.exit(f"sha256 mismatch for {source}")
    os.replace(part, dest)


if __name__ == "__main__":
    args = sys.argv[1:]
    if len(args) >= 5 and args[0] == "image":
        image(args[1], args[2], args[3], args[4:])
    elif len(args) == 4 and args[0] == "url":
        url(args[1], args[2], args[3])
    else:
        sys.exit(__doc__)
