#!/usr/bin/env python3
"""Download snugly's public benchmark corpus.

Every file comes from a public source listed in bench/corpus.tsv: camera sample photos,
HEIC/AVIF test images, sample videos, PDFs, Office files, the Silesia corpus, open-source
packages and release binaries. Each file is checked against its SHA-256 and is downloaded only
once; running the script again just verifies what is already there. Nothing personal is ever
involved, and none of these files are redistributed with snugly.

    python3 bench/fetch-corpus.py [--dir bench/corpus] [--only jpeg,pdf]

The files remain under their owners' licenses; they are fetched only for local testing.
"""

import argparse, concurrent.futures as cf, hashlib, io, os, sys, tarfile, urllib.request, zipfile

HERE = os.path.dirname(os.path.abspath(__file__))


def load(manifest):
    rows = []
    with open(manifest, encoding="utf-8") as f:
        for line in f:
            if line.startswith("#") or not line.strip():
                continue
            cat, name, unpack, size, sha, url = line.rstrip("\n").split("\t")
            rows.append(dict(cat=cat, name=name, unpack=unpack, size=int(size or 0), sha=sha, url=url))
    return rows


def sha256(b):
    return hashlib.sha256(b).hexdigest()


def safe_members(names, dest):
    for n in names:
        p = os.path.realpath(os.path.join(dest, n))
        if not p.startswith(os.path.realpath(dest) + os.sep) and p != os.path.realpath(dest):
            raise ValueError(f"unsafe path in archive: {n}")


def fetch(row, root):
    d = os.path.join(root, row["cat"])
    os.makedirs(d, exist_ok=True)
    target = os.path.join(d, row["name"])
    marker = os.path.join(d, "." + row["name"] + ".ok")
    if row["unpack"]:
        if os.path.exists(marker) and open(marker).read().strip() == row["sha"]:
            return row, "cached"
    elif os.path.exists(target) and (not row["sha"] or sha256(open(target, "rb").read()) == row["sha"]):
        return row, "cached"
    req = urllib.request.Request(row["url"], headers={"User-Agent": "snugly-bench"})
    data = urllib.request.urlopen(req, timeout=120).read()
    h = sha256(data)
    if row["sha"] and h != row["sha"]:
        raise ValueError(f"checksum mismatch for {row['url']}")
    row["sha"], row["size"] = h, len(data)
    if row["unpack"] == "zip":
        z = zipfile.ZipFile(io.BytesIO(data))
        safe_members(z.namelist(), target)
        z.extractall(target)
    elif row["unpack"] == "tgz":
        t = tarfile.open(fileobj=io.BytesIO(data), mode="r:gz")
        safe_members(t.getnames(), target)
        for m in t.getmembers():
            if m.issym() or m.islnk() or m.isdev():
                continue
            t.extract(m, target, **({"filter": "data"} if hasattr(tarfile, "data_filter") else {}))
    else:
        with open(target, "wb") as f:
            f.write(data)
    if row["unpack"]:
        with open(marker, "w") as f:
            f.write(h)
    return row, "downloaded"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dir", default=os.path.join(HERE, "corpus"))
    ap.add_argument("--manifest", default=os.path.join(HERE, "corpus.tsv"))
    ap.add_argument("--only", help="comma-separated categories")
    ap.add_argument("--write-manifest", action="store_true", help="record checksums of new entries (maintainers)")
    a = ap.parse_args()
    rows = load(a.manifest)
    todo = [r for r in rows if not a.only or r["cat"] in a.only.split(",")]
    done = errors = 0
    with cf.ThreadPoolExecutor(8) as ex:
        for fut in cf.as_completed([ex.submit(fetch, r, a.dir) for r in todo]):
            try:
                row, status = fut.result()
                done += status == "downloaded"
            except Exception as e:
                errors += 1
                print(f"error: {e}", file=sys.stderr)
    if a.write_manifest:
        with open(a.manifest, "w", encoding="utf-8") as f:
            f.write("# category\tname\tunpack\tbytes\tsha256\turl\n")
            for r in rows:
                f.write(f"{r['cat']}\t{r['name']}\t{r['unpack']}\t{r['size']}\t{r['sha']}\t{r['url']}\n")
    print(f"{len(todo)} entries: {done} downloaded, {len(todo) - done - errors} already present, {errors} errors -> {a.dir}")
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
