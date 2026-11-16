# Feature specification: Watch videos offline (048)

**Created**: 2026-11-16T22:20:00Z
**Status**: Slice 1 (web app) in progress. Phone app follows on the same port.
**Scope**: Download films and episodes in the web app and the phone app, and play them in the normal in-page player with no signal.
**Decision record**: ADR-0143 (the player's HLS stream, stored by exact URL).

---

## Problem statement

- **No signal, no video.** On a train or a plane the web app opens (its shell works offline) but nothing plays: every video streams.
- **Files don't play in a browser.** Many sources are MKV or HEVC, so saving the original file only helps with an external player.

## User stories

1. As a viewer, before a trip I pick a quality on a film's or episode's page and press Download; I see its progress and its size.
2. As a viewer with no signal, I open the app, see my downloads, press Play, and watch in the usual in-page player, with my subtitles.
3. As a viewer, I remove a download to free space, and I see how much of the cap my downloads use.
4. As a viewer, I save the original file to play in VLC (V1456).

## Requirements

- **FR-001**: A film's or episode's page offers Download, at Small (1 Mbps), Medium (2 Mbps, default) or Large (4 Mbps), wherever a video store is registered.
- **FR-002**: A download stores every response of the player's HLS stream at that bitrate under its exact URL, plus its text subtitles as WebVTT and the player's script.
- **FR-003**: A download is refused before fetching segments when its estimated size does not fit under the cap (4 GiB default) with what is kept, or would leave less than 2 GiB free; the refusal names both numbers.
- **FR-004**: Segments are fetched in order, two at a time, retrying a network error, 404 or 5xx with backoff; a download that fails names the file and status. A download cut off by closing the page resumes on the next start.
- **FR-005**: A downloaded item plays from the device, without calling the server, even when online. The player opens as for any video (the full in-page player).
- **FR-006**: The position is saved on the device while playing a download.
- **FR-007**: With no connection, the offline start lists ready downloads beside kept books, and Play opens the player.
- **FR-008**: A Downloads page lists each download's title, quality, size and state, with Play and Remove, and the total used of the cap.
- **FR-009**: Every decision (start, refusal, file failure, ready, offline play) is logged with the values it turned on.

## Out of scope (slice 1)

- Styled (ASS) subtitles offline; they fall back to WebVTT.
- Sending offline progress to the server when back online.
- Trickplay thumbnails offline.
