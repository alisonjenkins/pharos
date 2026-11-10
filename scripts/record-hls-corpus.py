#!/usr/bin/env python3
"""Record the pharos-hls test corpus (spec 031 T031-01) from a seeded pharos.

    scripts/record-hls-corpus.py http://127.0.0.1:8096 crates/pharos-hls/tests/corpus

The server is the one `just compat-dioxus-full on` starts: the seeded
`playwright` user, the 120 s `fixture-1.webm` (item ...01) and the test
media tree's `05-carefree.mp3` for the audio-only stream. For each
stream it saves the master and every media playlist. It also saves the init
and first media segment of the audio group, the first video rung and the
last (lowest) rung. The other rungs keep only their playlist, which keeps the
corpus under 2 MB. Tokens in the playlists are replaced with REDACTED.
"""

import json
import os
import re
import sys
import time
import urllib.parse
import urllib.request

ITEM = "00000000000000000000000000000001"
# A browser user agent: pharos gives browsers the demuxed fMP4 h264 master.
UA = "Mozilla/5.0 (X11; Linux x86_64) Firefox/155.0"
STREAMS = {
    "h264-cmaf": f"/videos/{ITEM}/master.m3u8?renditions=h264&abr=1&AudioStreamIndex=1",
    "vp9": f"/videos/{ITEM}/master.m3u8?renditions=vp9&AudioStreamIndex=1",
}


def main(base: str, out: str) -> None:
    auth = json.loads(
        fetch(
            base,
            "/Users/AuthenticateByName",
            data=json.dumps({"Username": "playwright", "Pw": "playwright-test-pw"}).encode(),
            headers={
                "Content-Type": "application/json",
                "X-Emby-Authorization": 'MediaBrowser Client="corpus", Device="corpus", DeviceId="corpus", Version="0"',
            },
        )
    )
    token = auth["AccessToken"]

    def get(url: str) -> bytes:
        return fetch(base, url, headers={"X-Emby-Token": token})

    streams = dict(STREAMS)
    streams["audio-only"] = f"/videos/{song(get, auth['User']['Id'])}/master.m3u8"
    for name, url in streams.items():
        master_url = urllib.parse.urljoin(base, url)
        master = get(master_url).decode()
        save(out, name, "master.m3u8", scrub(master).encode())
        media = uris(master)
        audio = [m for m in media if "audio" in m]
        video = [m for m in media if m not in audio]
        with_media = set(audio[:1] + video[:1] + video[-1:])
        for ref in media:
            playlist_url = urllib.parse.urljoin(master_url, ref)
            body = get(playlist_url).decode()
            save(out, name, local(playlist_url), scrub(body).encode())
            if ref not in with_media:
                continue
            refs = uris(body)
            inits = [r for r in refs if "init" in r.split("?")[0].rsplit("/", 1)[-1]]
            segments = [r for r in refs if r not in inits]
            for r in inits[:1] + segments[:1]:
                part = urllib.parse.urljoin(playlist_url, r)
                save(out, name, local(part), get(part))


def song(get, user_id: str) -> str:
    """The test media tree's mp3. The library scan finishes after the server
    answers, so wait for it."""
    query = f"/Users/{user_id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=Path"
    for _ in range(120):
        for item in json.loads(get(query))["Items"]:
            if (item.get("Path") or "").endswith("/05-carefree.mp3"):
                return item["Id"]
        time.sleep(1)
    sys.exit("record-hls-corpus: 05-carefree.mp3 never showed up in the library")


def fetch(base: str, url: str, data: bytes | None = None, headers: dict | None = None) -> bytes:
    req = urllib.request.Request(
        urllib.parse.urljoin(base, url), data=data, headers={"User-Agent": UA, **(headers or {})}
    )
    with urllib.request.urlopen(req, timeout=300) as r:
        return r.read()


def scrub(text: str) -> str:
    return re.sub(r"(api_key|PlaySessionId|MediaSourceId|DeviceId)=[^&\"\s]*", r"\1=REDACTED", text)


def local(url: str) -> str:
    """The path under the item, which is where the corpus keeps the file."""
    path = urllib.parse.urlparse(url).path
    return re.sub(r"^/videos/[0-9a-f]+/", "", path, flags=re.I).lstrip("/")


def uris(text: str) -> list[str]:
    lines = [line for line in text.splitlines() if line and not line.startswith("#")]
    return lines + re.findall(r'URI="([^"]+)"', text)


def save(out: str, stream: str, rel: str, data: bytes) -> None:
    path = os.path.join(out, stream, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    print(f"{stream}/{rel} {len(data)} bytes")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
