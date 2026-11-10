# ADR-0095: A device can be told not to direct-play an audio codec

- **Status:** Proposed
- **Date:** 2026-11-09T23:20:00Z (owner rule added 2026-11-10T08:20:00Z)
- **Deciders:** Alison Jenkins

## Context

Pharos believes what a client's DeviceProfile says it can play. The
User-Agent overrides that used to second-guess clients were removed on purpose,
and that rule stays.

T130 found the case it does not cover: a client whose claim is honest but
whose decoder is not. A Google TV's profile lists E-AC-3, so pharos
direct-plays A Beautiful Mind (h264 + E-AC-3 in Matroska) to it, and the TV's
own Dolby Digital Plus decoder plays the speech ahead of the picture. The same
file on a laptop, where ffmpeg decodes the audio, is in sync. Turning off
"Bitstream Dolby Digital" in the Jellyfin Android TV app fixes it on the
client, but not every client has that switch, and it has to be found on each
device.

## Decision

We will keep a per-device list of audio codecs that device must not be sent
as a bitstream, set by a person (an admin, or someone signed in on the
device) and never inferred.

- **Key:** the device id the session's token was issued to, so a TV and a
  laptop signed in as the same user are set apart. Stored as a JSON list in
  named config under `devaudionodirect:{device_id}`: no migration.
- **Where it applies:** the negotiator. The server copies the list onto the
  DeviceProfile (`denied_audio_codecs`, `#[serde(skip)]` so no client can send
  it) before `/Items/{id}/PlaybackInfo` and the native `Playback.negotiate`
  both decide, so the two APIs agree. A denied source codec fails the
  direct-play audio match even against an empty ("any") AudioCodec list, so
  the file takes the audio remux path (video copied, audio to AAC), and a
  transcode never picks a denied codec as its audio target.
- **Reason:** `DirectPlayBlock::AudioCodecDenied`, label `audio_codec_denied`
  on `pharos_playback_decision_total{direct_play_block}`, plus an info log
  `device audio codec deny list consulted` with the codec, the list and
  whether it applied.
- **Interface:** `POST /Devices/Options?id=` takes `NoDirectPlayAudioCodecs`
  (pharos extension; absent leaves it, `[]` clears it; names are lowercased,
  deduplicated, at most 16 of 1–16 `[a-z0-9_-]`; `aac` is refused because every
  conversion path, the HLS segment encoder included, produces AAC). `/Devices` lists it on each
  element that has one. Manage → Devices shows toggles for Dolby Digital,
  Dolby Digital Plus, Dolby TrueHD and DTS.
- **Who may set it (Alison, 2026-11-10):** an admin, for any device, and any
  user, for a device they are signed in on (they hold a token issued to that
  device id; a media or manage token does not count). The person holding the
  device is the one who notices a format playing badly, so making them ask an
  admin makes no sense. `/Devices/Options` accepts either and refuses anyone
  else 403 with nothing written; `/Devices` lists a non-admin's own devices
  instead of refusing. The native API gets `Session.myDevices` and
  `Session.setDeviceConvertedAudio` (schemaVersion 38) through the same owner
  rule, and Settings → Devices shows "Your devices" with the same toggles.
  The `device audio deny list set` log line carries `by.role` (`admin` or
  `owner`).

## Consequences

- A household fixes a mis-decoding device once, on the server, for every file.
- Every PlaybackInfo and native grant does two more small store reads (token
  to device, then the named config). No cache: both are point lookups, and a
  cache would hold memory for every device seen.
- `/Devices/Options` now writes each field only when the body carries it.
  Before, a body without `CustomName` cleared the name; jellyfin-web always
  sends it, so its behaviour is unchanged.
- A device that signs in again under a new device id loses its list. That is
  the cost of keying on the device rather than the user.
- The device id is the client's own claim. A user who signs in from
  anywhere under another person's device id becomes an owner of it and can
  change its list. The worst that does is convert a format that device could
  play, or stop converting one it plays badly; it never blocks playback or
  reaches another user's data, so we accept it rather than add device
  pairing.
- Not covered: video codecs. A follow-up if a need appears.

## Alternatives considered

- **Per user.** T130 suggested it, but the same user on a laptop plays the
  same file fine; a per-user list would transcode every device they own.
- **Rewriting the client's DirectPlayProfiles.** An empty AudioCodec list means
  "any", so stripping a codec from a list cannot express "any except eac3",
  and the transcode target would still come from the unedited
  TranscodingProfile.
- **A post-negotiation downgrade like `subtitle_burn`.** It would have to
  invent a target and would report the negotiator's `not_blocked` as the
  reason; deciding inside the negotiator reuses its tested remux fall-through.
- **Inferring it from the client or model name.** That is the User-Agent
  pattern the project removed.

## References

- `specs/001-pharos-baseline/tasks.md` T130; invariants V1010–V1012.
- `crates/pharos-jellyfin-api/src/device_profile.rs` (`denied_audio_codecs`,
  `AudioCodecDenied`), `crates/pharos-server/src/services/playback.rs`
  (`denied_audio_codecs_for_token`), `api/jellyfin/system.rs`
  (`device_options`, `devices_list`), `crates/pharos-server/src/services/devices.rs`
  (the owner rule), `api/pharos/rpc/session.rs` (`my_devices`,
  `set_device_converted_audio`), `crates/pharos-ui/src/views/admin.rs`
  (`DevicesPane`, `DeviceSoundFormats`), `crates/pharos-ui/src/native/my_devices.rs`.
- Invariant V1013 (who may set it).
