#!/usr/bin/env python3
"""Bumps pins in components.json to a GitHub release and fills in its SHA-256 digests.

    update_components.py loqdave                 newest release whose assets cover every platform
    update_components.py loqdave -t 2026.10.06   that release
    update_components.py --all                   every GitHub-hosted component
    update_components.py --check                 compare every pin against GitHub; changes nothing
    update_components.py -n loqdave              print the change as a diff instead of writing it

Digests come from the "digest" GitHub reports for each release asset, so nothing is downloaded
unless GitHub has none (assets uploaded before mid-2025) or the file is not a release asset at all
(a `files` entry whose url holds {tag}); those are downloaded once and hashed here.

"Newest" is the most recently published non-draft, non-prerelease release whose tag matches the
component's `track` regex (if any) and whose assets match every platform's `asset` template.
`"retained": "month-end"` narrows that to the last release of each finished month. It never asks
GitHub for /releases/latest: ENDEC_Dave keeps its SAPI zip marked Latest on purpose.

GH_TOKEN or GITHUB_TOKEN is used when set; without one GitHub allows 60 API requests an hour,
which is one or two per component.
"""

import argparse
import difflib
import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

MANIFEST = Path(__file__).resolve().parent / "components.json"
API = "https://api.github.com"
INLINE_WIDTH = 72


class UpdateError(Exception):
    pass


def request(url, accept="application/vnd.github+json"):
    headers = {"Accept": accept, "User-Agent": "eas-listener-update-components"}
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token and url.startswith(API):
        headers["Authorization"] = f"Bearer {token}"
    return urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=60)


def api_get(path):
    try:
        with request(API + path) as response:
            return json.load(response)
    except urllib.error.HTTPError as err:
        if err.code == 404:
            return None
        if err.code in (403, 429):
            raise UpdateError(f"GitHub refused {path} ({err.code}); set GH_TOKEN to lift the rate limit")
        raise


def sha256_of_url(url, cache):
    if url in cache:
        return cache[url]
    print(f"    hashing {url}", flush=True)
    digest = hashlib.sha256()
    with request(url, accept="application/octet-stream") as response:
        for chunk in iter(lambda: response.read(1 << 20), b""):
            digest.update(chunk)
    cache[url] = digest.hexdigest()
    return cache[url]


def subst(text, variables):
    for key, value in variables.items():
        text = text.replace("{" + key + "}", value)
    return text


def asset_pattern(template, tag, github, version):
    """A regex for an asset template. version=None captures {version} instead of fixing it."""
    pattern = ""
    captured = False
    for part in re.split(r"(\{tag\}|\{github\}|\{version\})", template):
        if part == "{tag}":
            pattern += re.escape(tag)
        elif part == "{github}":
            pattern += re.escape(github)
        elif part == "{version}" and version is not None:
            pattern += re.escape(version)
        elif part == "{version}":
            pattern += "(?P=version)" if captured else "(?P<version>.+?)"
            captured = True
        else:
            pattern += re.escape(part)
    return re.compile(pattern + r"\Z")


def derives_version(spec):
    return "version" in spec and any(
        "{version}" in p.get("asset", "") for p in spec.get("platforms", {}).values()
    )


def version_for(spec, tag):
    # An explicit version no asset name carries can only come from the tag: apprise-go's v0.3.3.
    if "version" not in spec:
        return tag
    return tag[1:] if re.match(r"v\d", tag) else tag


def match_release(spec, release):
    """Each platform's asset in this release and the version they agree on, or None."""
    tag = release["tag_name"]
    assets = {a["name"]: a for a in release.get("assets", [])}
    derive = derives_version(spec)
    version = None if derive else version_for(spec, tag)
    matched = {}
    for key, platform in spec.get("platforms", {}).items():
        if "asset" not in platform:
            continue
        pattern = asset_pattern(platform["asset"], tag, spec["github"], version)
        hits = [(name, pattern.match(name)) for name in assets if pattern.match(name)]
        if not hits:
            return None
        if len(hits) > 1:
            raise UpdateError(f"{tag}: {platform['asset']} matches {', '.join(n for n, _ in hits)}")
        name, found = hits[0]
        # The first platform to carry {version} fixes it; the rest must then match it exactly.
        if "version" in pattern.groupindex:
            version = found.group("version")
        matched[key] = assets[name]
    return version, matched


def pick_release(name, spec, tag):
    repo = spec["github"]
    if tag:
        release = api_get(f"/repos/{repo}/releases/tags/{tag}")
        if release is None:
            raise UpdateError(f"{repo} has no release tagged {tag}")
        result = match_release(spec, release)
        if result is None:
            raise UpdateError(f"{repo} {tag} is missing an asset for one of {name}'s platforms")
        return release, result

    track = re.compile(spec.get("track", ""))
    releases = api_get(f"/repos/{repo}/releases?per_page=100") or []
    releases = [
        r for r in releases
        if not r["draft"] and not r["prerelease"] and track.search(r["tag_name"])
    ]
    releases.sort(key=lambda r: r.get("published_at") or "", reverse=True)
    if spec.get("retained") == "month-end":
        # BtbN deletes daily builds after 14 days but keeps each month's last one for two years.
        # The current month's last build is not known until the month is over.
        this_month = datetime.now(timezone.utc).strftime("%Y-%m")
        month_ends = {}
        for release in releases:
            month = (release.get("published_at") or "")[:7]
            if month and month < this_month:
                month_ends.setdefault(month, release)
        releases = list(month_ends.values())
    for release in releases:
        result = match_release(spec, release)
        if result is not None:
            return release, result
    raise UpdateError(f"no release of {repo} has assets for every platform {name} pins")


def file_entries(spec):
    entries = list(spec.get("files", []))
    for platform in spec.get("platforms", {}).values():
        entries += platform.get("files", [])
    return entries


def update(name, spec, tag, cache):
    release, (version, matched) = pick_release(name, spec, tag)
    new_tag = release["tag_name"]
    old_tag = spec["release_tag"]
    print(f"{name}: {old_tag} -> {new_tag}" if new_tag != old_tag else f"{name}: {new_tag} (unchanged tag)")

    for key, asset in matched.items():
        platform = spec["platforms"][key]
        digest = asset.get("digest") or ""
        if digest.startswith("sha256:"):
            sha = digest[len("sha256:"):]
        elif new_tag == old_tag and platform.get("sha256"):
            sha = platform["sha256"]
        else:
            sha = sha256_of_url(asset["browser_download_url"], cache)
        if platform.get("sha256") != sha:
            print(f"  {key:<15} {asset['name']}  {sha}")
        platform["sha256"] = sha

    variables = {"tag": new_tag, "version": version, "github": spec["github"]}
    for entry in file_entries(spec):
        if "{" not in entry["url"]:
            continue
        if new_tag == old_tag and entry.get("sha256"):
            continue
        sha = sha256_of_url(subst(entry["url"], variables), cache)
        if entry.get("sha256") != sha:
            print(f"  {entry['to']:<15}  {sha}")
        entry["sha256"] = sha

    spec["release_tag"] = new_tag
    if "version" in spec:
        spec["version"] = version


def check(name, spec):
    """Problems with the pinned release as GitHub reports it now."""
    repo, tag = spec["github"], spec["release_tag"]
    release = api_get(f"/repos/{repo}/releases/tags/{tag}")
    if release is None:
        return [f"{repo} no longer has a release tagged {tag}"]
    assets = {a["name"]: a for a in release.get("assets", [])}
    version = spec.get("version", tag)
    problems = []
    for key, platform in spec.get("platforms", {}).items():
        if "asset" not in platform:
            continue
        asset_name = subst(platform["asset"], {"tag": tag, "version": version, "github": repo})
        asset = assets.get(asset_name)
        if asset is None:
            problems.append(f"{key}: {asset_name} is not in {tag}")
            continue
        digest = asset.get("digest") or ""
        if not digest.startswith("sha256:"):
            print(f"  {name} {key}: GitHub has no digest for {asset_name}; not verified")
        elif digest[len("sha256:"):] != platform.get("sha256"):
            problems.append(f"{key}: pinned {platform.get('sha256')}, GitHub has {digest[len('sha256:'):]}")
    return problems


def render(value, indent=0):
    """JSON in the manifest's layout: 4 spaces, short scalar lists and objects kept on one line."""
    pad = " " * indent
    inner = " " * (indent + 4)
    dump = lambda v: json.dumps(v, ensure_ascii=False)
    if isinstance(value, dict) and value:
        if all(not isinstance(v, (dict, list)) for v in value.values()):
            line = "{ " + ", ".join(f"{dump(k)}: {dump(v)}" for k, v in value.items()) + " }"
            if len(line) <= INLINE_WIDTH:
                return line
        items = [f"{inner}{dump(k)}: {render(v, indent + 4)}" for k, v in value.items()]
        return "{\n" + ",\n".join(items) + "\n" + pad + "}"
    if isinstance(value, list) and value:
        if all(not isinstance(v, (dict, list)) for v in value):
            return "[" + ", ".join(dump(v) for v in value) + "]"
        items = [inner + render(v, indent + 4) for v in value]
        return "[\n" + ",\n".join(items) + "\n" + pad + "]"
    return dump(value)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("components", nargs="*", help="manifest names, e.g. loqdave")
    parser.add_argument("-t", "--tag", help="pin this release tag instead of the newest")
    parser.add_argument("-a", "--all", action="store_true", help="every GitHub-hosted component")
    parser.add_argument("-c", "--check", action="store_true", help="verify pins against GitHub only")
    parser.add_argument("-n", "--dry-run", action="store_true", help="print a diff instead of writing")
    args = parser.parse_args()

    original = MANIFEST.read_text(encoding="utf-8")
    manifest = json.loads(original)
    components = manifest["components"]
    hosted = [n for n, s in components.items() if s.get("github") and s.get("release_tag")]

    names = args.components or (hosted if args.all or args.check else [])
    if not names:
        parser.error("name the components to update, or pass --all")
    for name in names:
        if name not in hosted:
            parser.error(f"{name} is not a GitHub-hosted component; known: {', '.join(hosted)}")
    if args.tag and len(names) != 1:
        parser.error("--tag needs exactly one component")

    try:
        if args.check:
            failed = False
            for name in names:
                problems = check(name, components[name])
                print(f"{name} {components[name]['release_tag']}: {'ok' if not problems else 'FAILED'}")
                for problem in problems:
                    print(f"  {problem}")
                failed |= bool(problems)
            return 1 if failed else 0

        cache = {}
        for name in names:
            update(name, components[name], args.tag, cache)
    except UpdateError as err:
        print(f"update_components: {err}", file=sys.stderr)
        return 1

    rendered = render(manifest) + "\n"
    if rendered == original:
        print("components.json is already up to date")
    elif args.dry_run:
        sys.stdout.writelines(difflib.unified_diff(
            original.splitlines(keepends=True), rendered.splitlines(keepends=True),
            "a/tools/components.json", "b/tools/components.json"))
    else:
        with open(MANIFEST, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(rendered)
        print(f"wrote {MANIFEST}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
