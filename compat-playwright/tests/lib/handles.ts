// Verified control handles for the nix-pinned jellyfin-web 10.11.8 bundle
// (JELLYFIN_WEB_DIR). Handles are deterministic per pinned input; if the
// jellyfin-web pin bumps, re-verify by grepping the bundle chunks:
//   - login/server:   #txtServerHost / #txtManualName / #txtManualPassword
//   - play button:    button.btnPlay (item details page)
//   - OSD track btns:  .btnAudio / .btnSubtitles  (playback-video chunk)
//   - track menu:      .actionSheetMenuItem[data-id="<streamIndex>"]
//                      (actionsheet chunk renders data-id="<Id>")
//
// window.ApiClient exposes createSyncPlayGroup / joinSyncPlayGroup /
// leaveSyncPlayGroup. window.playbackManager is NOT exposed, so group
// commands are issued via each member's own /SyncPlay/* REST call (see
// virtual-person.ts) and audio/subtitle swaps drive the real OSD menus.

export const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
export const WS_URL = PHAROS_URL.replace(/^http/, "ws");

export interface Credentials {
  user: string;
  pass: string;
}

export const USERS: Credentials[] = [
  {
    user: process.env.PHAROS_TEST_USER ?? "playwright",
    pass: process.env.PHAROS_TEST_PASS ?? "playwright-test-pw",
  },
  {
    user: process.env.PHAROS_TEST_USER2 ?? "playwright2",
    pass: process.env.PHAROS_TEST_PASS2 ?? "playwright2-test-pw",
  },
  {
    user: process.env.PHAROS_TEST_USER3 ?? "playwright3",
    pass: process.env.PHAROS_TEST_PASS3 ?? "playwright3-test-pw",
  },
];

export const SELECTORS = {
  serverHost: "#txtServerHost",
  manualName: "#txtManualName",
  manualPassword: "#txtManualPassword",
  signIn: /^sign in$/i,
  addServer: /add server/i,
  connect: /^connect$/i,
  playButton: "button.btnPlay",
  videoOsd: ".videoOsdBottom",
  osdAudioButton: ".btnAudio",
  osdSubtitleButton: ".btnSubtitles",
  actionSheet: ".actionSheet",
  // Track actionsheet items carry data-id = the MediaStream index.
  trackMenuItem: (index: number) => `.actionSheetMenuItem[data-id="${index}"]`,
} as const;
