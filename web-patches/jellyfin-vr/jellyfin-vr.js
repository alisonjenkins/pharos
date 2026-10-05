/*
 * Jellyfin supports plugins for VR v1
 */
(function () {
  'use strict';

  if (window.__JELLYFIN_VR_V42__) return;
  window.__JELLYFIN_VR_V42__ = true;

  const THREE_URL = 'https://cdn.jsdelivr.net/npm/three@0.160.1/build/three.min.js';
  const MENU_ITEM_ID = 'jvr-menu-item';
  const MENU_ITEM_LABEL = 'Watch in VR';
  // Jellyfin renders every popup menu, including the video OSD's settings
  // menu, through its actionSheet component.
  const SHEET_SELECTOR = '.actionSheet,.actionsheet';
  const SHEET_ITEM_SELECTOR = '.actionSheetMenuItem,.listItem-button';
  const OVERLAY_ID = 'jvr-v4-overlay';
  const PANEL_WIDTH = 1.82;
  const PANEL_HEIGHT = 0.82;
  const PANEL_HOME = { x: 0, y: -0.13, z: -1.32 };
  const TIMELINE = { x: 80, y: 164, width: 1440, height: 76, trackY: 192, trackHeight: 20 };
  const PROGRESS_INTERVAL = 10000;
  const MODE_STORE_KEY = 'jvr.modes.v1';
  const MODE_STORE_LIMIT = 200;
  const METADATA_TIMEOUT = 2500;
  const ENV_STORE_KEY = 'jvr.env.v1';
  const ENVIRONMENTS = ['void', 'theater'];
  const QUALITY_STORE_KEY = 'jvr.quality.v1';
  // The compatibility source is our own transcode, so "quality" is a resolution
  // cap plus a bitrate ceiling. A VR frame carries both eyes in one picture, so
  // the ladder is named by the packed frame width rather than by a 16:9
  // marketing tier: "4K" here is a 3840-wide frame, which is only 1920 per eye
  // once side-by-side is unpacked. 4096 is the practical ceiling — the largest
  // frame the H.264 decoders in standalone headsets reliably accept.
  const QUALITY_PRESETS = [
    { id: 'max', label: 'MAX', cap: 4096, bitrate: 60000000, note: 'Source resolution, capped at 4096px' },
    { id: '4k', label: '4K', cap: 3840, bitrate: 40000000, note: '3840px frame · 40 Mbps' },
    { id: '2.5k', label: '2.5K', cap: 2560, bitrate: 20000000, note: '2560px frame · 20 Mbps' },
    { id: '1080', label: '1080', cap: 1920, bitrate: 10000000, note: '1920px frame · 10 Mbps' },
    { id: '720', label: '720', cap: 1280, bitrate: 4000000, note: '1280px frame · 4 Mbps' }
  ];
  const LIGHT_STORE_KEY = 'jvr.lights.v1';
  // House lighting presets, as multipliers on what the room is built with.
  //
  // `spill` is the screen's own light — the key, the stage bounce and the wash
  // on the masking wall. It never goes above 1, so raising the house lights
  // cannot make the picture appear to glow harder, but it does fall below 1 at
  // the watching levels: in a headset the eye adapts to the dark far more than
  // a monitor lets it, and at full spill the front rows stayed plainly lit with
  // the house out. `exit` is the EXIT signs. They are never off in a real
  // auditorium, but at blackout they should be a faint green smudge you find
  // when you look for it, not a lamp in the corner of your eye.
  //
  // The room is authored at roughly house-lights-up brightness, so every level
  // that is not FULL scales it down — LOW is a viewing level, not the as-built
  // state. That matters: LOW is where a film is actually watched, so the room
  // has to fall back far enough to stop competing with the screen. Walls, seats
  // and fixtures stay legible, but as shapes in a dark room rather than as
  // things worth looking at. The glow sprites on the sconces, aisle markers and
  // downlights come down less than the lights that wash the walls, so the
  // fixtures still read as fixtures once the surfaces around them go dark.
  //
  // The four are really two pairs. OUT and LOW are both watching levels — a
  // blackout and a dim room — and sit close together well under 1. HALF and
  // FULL are house-lights-up and sit above it. The gap in the middle is the
  // point: there is no setting that half-lights the room while a film runs.
  //
  // `fog` thins the haze as the lights come up — a lit room with blackout-level
  // haze in it reads as smoke, not as air.
  const LIGHT_LEVELS = [
    { id: 'out', label: 'OUT', ambient: 0.008, house: 0.004, sconce: 0.1, downlight: 0.025, spill: 0.3, exit: 0.12, fog: 1.35 },
    { id: 'low', label: 'LOW', ambient: 0.06, house: 0.04, sconce: 0.3, downlight: 0.15, spill: 0.55, exit: 0.3, fog: 1.15 },
    { id: 'half', label: 'HALF', ambient: 1.5, house: 1.7, sconce: 1.2, downlight: 1.4, spill: 1, exit: 0.7, fog: 0.88 },
    { id: 'full', label: 'FULL', ambient: 3.8, house: 4.6, sconce: 2.2, downlight: 4, spill: 1, exit: 1, fog: 0.6 }
  ];
  const SEAT_STORE_KEY = 'jvr.seat.v1';
  // Where in the rake the viewer sits, as a seat row. Row 0 is the default spot
  // the room is laid out around, and the four rows are an even 2.5m ladder:
  // 8.1m to 16.9m of throw off a screen 8.8m tall. The rake itself runs to row
  // -8, but the offered seats stop well short of it — from the true front row
  // the top of the picture is 60 degrees up, which is not a seat, it is a stunt.
  const SEATS = [
    { id: 'front', label: 'FRONT', row: -4 },
    { id: 'close', label: 'CLOSE', row: -2 },
    { id: 'mid', label: 'MID', row: 0 },
    { id: 'back', label: 'BACK', row: 3 }
  ];
  // Panel button rows, in canvas pixels. Widths are derived per row so adding a
  // button does not mean re-deriving every x by hand.
  const PANEL_ROW = { left: 55, width: 1490, gap: 18, height: 102, y: [250, 372, 494] };
  const EYE_HEIGHT = 1.6;
  // How far above or below the horizon the head has to be looking for a
  // recentre to tip the whole room onto the gaze instead of only turning it.
  // Sitting up, nobody recentres while looking 35 degrees up at a screen that
  // is straight ahead; lying back on a bed or a sofa, the gaze is 60 to 90.
  const RECLINE_PITCH = (35 * Math.PI) / 180;
  // Screen placement per environment, in videoRoot-local space (origin at eye
  // height). `curveRatio` is the cylinder radius as a multiple of the viewing
  // distance; 2.2 gives a sagitta of ~7% of the screen width, which is the
  // gentle curve a commercial cinema screen actually has. 0 stays flat.
  const SCREEN_LAYOUTS = {
    void: { height: 4.05, maxWidth: 9, distance: 4.5, centerY: 0, curveRatio: 0 },
    theater: { height: 8.8, maxWidth: 17, distance: 13.1, centerY: 0.12, curveRatio: 2.2 }
  };
  // Auditorium dimensions, in environment-local space: y = 0 is the tread the
  // viewer stands on, the viewer is at the origin, and the screen is down -z.
  const THEATER = {
    rowDepth: 1.25,
    riser: 0.42,
    rowsFront: 8,
    rowsBack: 4,
    seatPitch: 0.58,
    seatHalfSpan: 8.2,
    aisleInner: 2.7,
    aisleOuter: 4,
    standingGap: 0.9,
    // Seat rows arc around a point far behind the screen. The radius has to stay
    // large enough that the outer seats of a row do not slide off their own
    // tread: the offset grows as span^2 / 2r, and the tread is only rowDepth deep.
    arcCenterZ: -110,
    wallX: 9.8,
    ceilingY: 8,
    backWallZ: 7.8,
    screenWallZ: -13.6,
    stageHeight: 0.7
  };

  let overlay = null;
  let statusEl = null;
  let sourceVideo = null;
  let activeVideo = null;
  let compatVideo = null;
  let originalState = null;
  let compatUrl = '';
  let compatOffset = 0;
  let sourceMode = 'original';
  let qualityId = 'max';
  let savedVideoId = '';
  let loadSerial = 0;
  let jellyfin = null;
  let compatSessionId = '';
  let progressTimer = 0;
  let lastReportKey = '';
  let itemText = '';
  let itemTextReady = false;
  let detectionDone = false;
  let detectionTimer = 0;
  let detectionMessage = '';

  let world = null;
  let camera = null;
  let renderer = null;
  let videoTexture = null;
  let videoRoot = null;
  let videoMeshes = [];
  let environmentName = 'theater';
  let environmentRoot = null;
  let environmentBuilt = '';
  let seatRoot = null;
  let lightLevelId = 'low';
  let seatId = 'mid';
  // Where the view is aimed: a base orientation set by recentring, with the
  // trigger-drag yaw and pitch on top of it. Upright the base is null and the
  // room stays level; reclined it is the head pose the viewer recentred on.
  let viewBase = null;
  let viewYaw = 0;
  let viewPitch = 0;
  let screenSurround = null;
  let environmentFog = null;
  let screenLayout = { width: 7.2, height: 4.05, y: 0, z: -4.5 };
  let controllers = [];
  let panelMesh = null;
  let panelCanvas = null;
  let panelContext = null;
  let panelTexture = null;
  let panelBorder = null;
  let panelCursor = null;
  let panelVisible = false;
  let panelButtons = [];
  let hoverAction = '';
  let lastPanelDraw = 0;
  let lastStickAction = 0;
  let xrSession = null;
  let closing = false;

  let scanTimer = 0;
  let scanScheduled = false;
  let resizeHandler = null;
  const inputStates = new WeakMap();
  const triggerStates = new WeakMap();
  const gripTimers = new WeakMap();
  const dragState = {
    controller: null,
    type: '',
    startTime: 0,
    startDirection: null,
    startYaw: 0,
    startPitch: 0,
    startPanelPosition: null,
    startPanelQuaternion: null,
    seekTime: 0,
    moved: false
  };
  const currentMode = { projection: '180', stereo: 'sbs', swap: false };

  function loadScript(src) {
    return new Promise((resolve, reject) => {
      if (window.THREE?.WebGLRenderer) return resolve();
      const existing = Array.from(document.scripts).find((item) => item.src === src);
      const script = existing || document.createElement('script');
      if (!existing) {
        script.src = src;
        script.crossOrigin = 'anonymous';
        document.head.appendChild(script);
      }
      script.addEventListener('load', resolve, { once: true });
      script.addEventListener('error', () => reject(new Error('Failed to load Three.js')), { once: true });
    });
  }

  async function ensureThree() {
    if (!window.THREE?.WebGLRenderer) await loadScript(THREE_URL);
    if (!window.THREE?.WebGLRenderer) throw new Error('Three.js failed to initialise');
  }

  function findVideo() {
    const videos = Array.from(document.querySelectorAll('video')).filter((video) => video.id !== 'jvr-compat-video');
    return videos.find((video) => video.currentSrc && !video.paused)
      || videos.find((video) => video.currentSrc || video.src)
      || null;
  }

  function formatTime(value) {
    if (!Number.isFinite(value)) return '0:00';
    const hours = Math.floor(value / 3600);
    const minutes = Math.floor((value % 3600) / 60);
    const seconds = Math.floor(value % 60).toString().padStart(2, '0');
    return hours
      ? `${hours}:${minutes.toString().padStart(2, '0')}:${seconds}`
      : `${minutes}:${seconds}`;
  }

  function setQueryParam(url, name, value) {
    Array.from(url.searchParams.keys()).forEach((key) => {
      if (key.toLowerCase() === name.toLowerCase()) url.searchParams.delete(key);
    });
    if (value !== null && value !== undefined) url.searchParams.set(name, String(value));
  }

  function readQueryParam(url, name) {
    const key = Array.from(url.searchParams.keys()).find((item) => item.toLowerCase() === name.toLowerCase());
    return key ? url.searchParams.get(key) || '' : '';
  }

  // Everything needed to talk to the Jellyfin API is already present in the
  // stream URL Jellyfin built for its own player; ApiClient only fills gaps.
  function readJellyfinContext(video) {
    const raw = video?.currentSrc || video?.src;
    if (!raw) return null;
    let url;
    try {
      url = new URL(raw, location.href);
    } catch (_) {
      return null;
    }
    const match = url.pathname.match(/^(.*)\/Videos\/([^/]+)\/stream(?:\.[^/]*)?$/i);
    if (!match) return null;
    const context = {
      base: `${url.origin}${match[1]}`,
      itemId: match[2],
      mediaSourceId: readQueryParam(url, 'MediaSourceId') || match[2],
      playSessionId: readQueryParam(url, 'PlaySessionId'),
      apiKey: readQueryParam(url, 'api_key') || readQueryParam(url, 'ApiKey'),
      deviceId: readQueryParam(url, 'DeviceId')
    };
    const client = window.ApiClient;
    if (client) {
      try { if (!context.apiKey) context.apiKey = client.accessToken?.() || ''; } catch (_) { }
      try { if (!context.deviceId) context.deviceId = client.deviceId?.() || ''; } catch (_) { }
    }
    return context.apiKey ? context : null;
  }

  async function jellyfinRequest(method, path, body) {
    if (!jellyfin?.apiKey) return false;
    const headers = { 'X-Emby-Token': jellyfin.apiKey };
    if (body) headers['Content-Type'] = 'application/json';
    try {
      // keepalive so teardown requests still leave during page hide/unload.
      const response = await fetch(`${jellyfin.base}${path}`, {
        method,
        headers,
        body: body ? JSON.stringify(body) : undefined,
        keepalive: true
      });
      return response.ok;
    } catch (_) {
      return false;
    }
  }

  // Jellyfin stops advancing watch history while its own <video> is parked, so
  // the compat source has to report its position itself.
  function reportProgress(eventName) {
    if (sourceMode !== 'compat' || !jellyfin?.playSessionId || !activeVideo) return;
    const ticks = Math.max(0, Math.round(getCurrentTime() * 10000000));
    const paused = Boolean(activeVideo.paused);
    const key = `${ticks}|${paused}`;
    if (key === lastReportKey) return;
    lastReportKey = key;
    jellyfinRequest('POST', '/Sessions/Playing/Progress', {
      ItemId: jellyfin.itemId,
      MediaSourceId: jellyfin.mediaSourceId,
      PlaySessionId: jellyfin.playSessionId,
      PositionTicks: ticks,
      IsPaused: paused,
      IsMuted: Boolean(activeVideo.muted),
      VolumeLevel: Math.round((activeVideo.volume ?? 1) * 100),
      CanSeek: true,
      PlayMethod: 'Transcode',
      EventName: eventName || 'timeupdate'
    });
  }

  function startProgressReporting() {
    stopProgressReporting();
    progressTimer = setInterval(() => reportProgress('timeupdate'), PROGRESS_INTERVAL);
  }

  function stopProgressReporting() {
    if (progressTimer) clearInterval(progressTimer);
    progressTimer = 0;
  }

  function newCompatSessionId() {
    let random = '';
    try { random = crypto.randomUUID().replace(/-/g, ''); } catch (_) { }
    if (!random) random = `${Date.now().toString(16)}${Math.random().toString(16).slice(2)}`;
    return `jvr${random}`.slice(0, 40);
  }

  // Only ever kills our own transcode: the id passed here is always one we
  // generated, never Jellyfin's PlaySessionId, so this cannot stop the
  // original stream the Jellyfin player is using.
  function stopEncoding(sessionId) {
    if (!sessionId) return;
    const params = new URLSearchParams();
    if (jellyfin?.deviceId) params.set('deviceId', jellyfin.deviceId);
    params.set('playSessionId', sessionId);
    jellyfinRequest('DELETE', `/Videos/ActiveEncodings?${params.toString()}`);
  }

  function stopCompatEncoding() {
    stopEncoding(compatSessionId);
    compatSessionId = '';
  }

  // ---------------------------------------------------------------------
  // Mode auto-detection
  //
  // Priority: a mode the user saved for this item > markers in the item name
  // or file path > aspect-ratio heuristics. Name markers have to win, because
  // half-SBS and half-OU are squeezed back into an ordinary 16:9 frame and are
  // therefore indistinguishable from a 2D video by aspect alone.
  // ---------------------------------------------------------------------

  const PROJECTION_TOKENS = {
    '360': '360', vr360: '360', '360vr': '360', '360x180': '360',
    '180': '180', vr180: '180', '180vr': '180', '180x180': '180',
    fisheye: 'fisheye',
    flat: 'flat', '2d': 'flat'
  };
  const STEREO_TOKENS = {
    sbs: 'sbs', hsbs: 'sbs', fsbs: 'sbs', '3dsbs': 'sbs', lr: 'sbs',
    ou: 'ou', hou: 'ou', tb: 'ou', htb: 'ou', tab: 'ou',
    mono: 'mono', '2d': 'mono'
  };
  // Lens profiles imply a fisheye projection: MKX200, MKX220, RF52, VRCA220...
  const LENS_TOKEN = /^(?:mkx|rf|vrca|fisheye)\d+$/;

  function tokenize(text) {
    return String(text || '')
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, ' ')
      .trim()
      .split(' ')
      .filter(Boolean);
  }

  // Packing markers show up glued to qualifiers: HalfOU, FullSBS, SBS3D, 3DTB.
  function stereoFromToken(token) {
    if (STEREO_TOKENS[token]) return STEREO_TOKENS[token];
    const stripped = token.replace(/^(?:half|full|3d)/, '').replace(/3d$/, '');
    return stripped && stripped !== token ? STEREO_TOKENS[stripped] || '' : '';
  }

  function detectFromText(text) {
    const result = { projection: '', stereo: '', swap: false, stereoHint: false };
    for (const token of tokenize(text)) {
      if (!result.projection && PROJECTION_TOKENS[token]) result.projection = PROJECTION_TOKENS[token];
      if (!result.projection && LENS_TOKEN.test(token)) result.projection = 'fisheye';
      if (!result.stereo) result.stereo = stereoFromToken(token);
      if (token === 'rl') { result.stereo = result.stereo || 'sbs'; result.swap = true; }
      if (/^3d$/.test(token) || /3d$/.test(token) || /^3d/.test(token)) result.stereoHint = true;
    }
    return result;
  }

  function detectFromAspect(width, height) {
    if (!width || !height) return {};
    const ratio = width / height;
    const near = (target, tolerance) => Math.abs(ratio - target) <= tolerance;
    // VR panoramas are wide *and* large; gate on width so that a 2.00:1 or
    // 2.35:1 cinema scope master is not mistaken for 180 side-by-side.
    if (near(4, 0.2) && width >= 3000) return { projection: '360', stereo: 'sbs' };
    if (near(3.56, 0.2)) return { projection: 'flat', stereo: 'sbs' };
    if (near(2, 0.12) && width >= 3000) return { projection: '180', stereo: 'sbs' };
    if (near(1, 0.1) && width >= 2000) return { projection: '180', stereo: 'mono' };
    if (near(0.5, 0.06)) return { projection: '180', stereo: 'ou' };
    if (near(0.89, 0.06)) return { projection: 'flat', stereo: 'ou' };
    if (ratio >= 1.6 && ratio <= 2.45) return { projection: 'flat', stereo: 'mono' };
    return {};
  }

  function resolveMode(text, width, height) {
    const fromText = detectFromText(text);
    const fromAspect = detectFromAspect(width, height);
    const projection = fromText.projection || fromAspect.projection;
    // A bare "3D" marker says there are two eyes but not how they are packed;
    // side-by-side is by far the more common packing. It has to outrank an
    // aspect-derived "mono", because half-SBS and half-OU are squeezed back
    // into an ordinary 16:9 frame where the aspect ratio sees only one eye.
    let stereo = fromText.stereo || fromAspect.stereo || '';
    if (!fromText.stereo && fromText.stereoHint && (!stereo || stereo === 'mono')) stereo = 'sbs';
    if (!projection && !stereo) return null;
    const source = fromText.projection || fromText.stereo || fromText.stereoHint ? 'name' : 'resolution';
    return {
      projection: projection || (stereo && stereo !== 'mono' ? '180' : 'flat'),
      stereo: stereo || 'mono',
      swap: fromText.swap,
      source
    };
  }

  function readStoredModes() {
    try {
      return JSON.parse(localStorage.getItem(MODE_STORE_KEY)) || {};
    } catch (_) {
      return {};
    }
  }

  function rememberMode() {
    if (!jellyfin?.itemId) return;
    try {
      const store = readStoredModes();
      delete store[jellyfin.itemId];
      store[jellyfin.itemId] = {
        projection: currentMode.projection,
        stereo: currentMode.stereo,
        swap: currentMode.swap
      };
      const keys = Object.keys(store);
      for (const key of keys.slice(0, Math.max(0, keys.length - MODE_STORE_LIMIT))) delete store[key];
      localStorage.setItem(MODE_STORE_KEY, JSON.stringify(store));
    } catch (_) { }
  }

  function applyMode(changes, remember) {
    Object.assign(currentMode, changes);
    rebuildVideoMeshes();
    if (remember) rememberMode();
  }

  function autoDetectMode() {
    if (detectionDone || !itemTextReady) return;
    const width = sourceVideo?.videoWidth || activeVideo?.videoWidth || 0;
    const height = sourceVideo?.videoHeight || activeVideo?.videoHeight || 0;
    if (!width || !height) return;
    detectionDone = true;
    if (detectionTimer) { clearTimeout(detectionTimer); detectionTimer = 0; }

    const stored = jellyfin?.itemId ? readStoredModes()[jellyfin.itemId] : null;
    if (stored?.projection && stored?.stereo) {
      applyMode({ projection: stored.projection, stereo: stored.stereo, swap: Boolean(stored.swap) }, false);
      detectionMessage = `saved mode ${stored.projection.toUpperCase()} / ${stored.stereo.toUpperCase()}`;
    } else {
      const detected = resolveMode(itemText, width, height);
      if (!detected) return;
      applyMode({ projection: detected.projection, stereo: detected.stereo, swap: detected.swap }, false);
      detectionMessage = `detected ${detected.projection.toUpperCase()} / ${detected.stereo.toUpperCase()} from ${detected.source === 'name' ? 'the file name' : 'the resolution'}`;
    }
    if (statusEl) statusEl.textContent = `Mode: ${detectionMessage}`;
  }

  async function jellyfinGetJson(path) {
    if (!jellyfin?.apiKey) return null;
    try {
      const response = await fetch(`${jellyfin.base}${path}`, { headers: { 'X-Emby-Token': jellyfin.apiKey } });
      if (!response.ok) return null;
      return await response.json();
    } catch (_) {
      return null;
    }
  }

  // The stream URL carries no file name, so the markers detection needs live
  // in the item record. Endpoint shape moved across Jellyfin versions, so try
  // the user-scoped route first and fall back to the flat one.
  async function loadItemText() {
    itemText = document.title || '';
    if (jellyfin?.itemId) {
      let userId = '';
      try { userId = window.ApiClient?.getCurrentUserId?.() || ''; } catch (_) { }
      const routes = [];
      if (userId) routes.push(`/Users/${userId}/Items/${jellyfin.itemId}`);
      routes.push(`/Items/${jellyfin.itemId}`);
      for (const route of routes) {
        const item = await jellyfinGetJson(route);
        if (!item) continue;
        const sources = Array.isArray(item.MediaSources) ? item.MediaSources : [];
        const parts = [item.Name, item.OriginalTitle, item.Path]
          .concat(sources.map((source) => source.Name))
          .concat(sources.map((source) => source.Path))
          .filter(Boolean);
        if (parts.length) itemText = parts.join(' ');
        break;
      }
    }
    itemTextReady = true;
    autoDetectMode();
  }

  function buildCompatStreamUrl(video) {
    const raw = video?.currentSrc || video?.src;
    if (!raw) return '';
    try {
      const url = new URL(raw, location.href);
      const match = url.pathname.match(/^(.*\/Videos\/)([^/]+)\/stream(?:\.[^/]*)?$/i);
      if (!match) return '';
      url.pathname = `${match[1]}${match[2]}/stream.mp4`;
      setQueryParam(url, 'Static', 'false');
      setQueryParam(url, 'VideoCodec', 'h264');
      setQueryParam(url, 'AudioCodec', 'aac');
      setQueryParam(url, 'EnableAutoStreamCopy', 'false');
      setQueryParam(url, 'AllowVideoStreamCopy', 'false');
      setQueryParam(url, 'AllowAudioStreamCopy', 'false');
      setQueryParam(url, 'MaxVideoBitDepth', '8');
      setQueryParam(url, 'RequireAvc', 'true');
      setQueryParam(url, 'AudioBitRate', '320000');
      setQueryParam(url, 'AudioChannels', '2');
      // Resolution and bitrate are applied per request by applyQualityParams,
      // so changing quality only costs a transcode restart, not a rebuild.
      setQueryParam(url, 'Context', 'Streaming');
      setQueryParam(url, 'SubtitleStreamIndex', null);
      setQueryParam(url, 'SubtitleMethod', null);
      setQueryParam(url, 'StartTimeTicks', null);
      return url.href;
    } catch (_) {
      return '';
    }
  }

  function getCurrentTime() {
    if (!activeVideo) return 0;
    return sourceMode === 'compat'
      ? compatOffset + (activeVideo.currentTime || 0)
      : (activeVideo.currentTime || 0);
  }

  function getDuration() {
    return originalState?.duration || sourceVideo?.duration || activeVideo?.duration || 0;
  }

  function addStyles() {
    if (document.getElementById('jvr-v4-style')) return;
    const style = document.createElement('style');
    style.id = 'jvr-v4-style';
    style.textContent = `
      #${OVERLAY_ID}{position:fixed;inset:0;z-index:2147483646;background:#000;color:#fff;font-family:system-ui,-apple-system,"Segoe UI",sans-serif;overflow:hidden}
      #${OVERLAY_ID} .jvr-canvas{position:absolute;inset:0;width:100%;height:100%;display:block}
      #${OVERLAY_ID} .jvr-bar{position:absolute;top:0;left:0;right:0;z-index:5;display:flex;align-items:center;gap:7px;padding:10px;background:linear-gradient(#000e,transparent);pointer-events:auto}
      #${OVERLAY_ID} button{border:1px solid #ffffff45;border-radius:7px;background:#17191ddd;color:#fff;padding:8px 10px;font:inherit;cursor:pointer}
      #${OVERLAY_ID} button.active{border-color:#20b9ed;background:#075c78}
      #${OVERLAY_ID} button:disabled{opacity:.4;cursor:not-allowed}
      #${OVERLAY_ID} select{border:1px solid #ffffff45;border-radius:7px;background:#17191ddd;color:#fff;padding:8px 10px;font:inherit;cursor:pointer}
      #${OVERLAY_ID} select:disabled{opacity:.4;cursor:not-allowed}
      #${OVERLAY_ID} .jvr-enter{margin-left:auto;background:#008fbd;font-weight:700}
      #${OVERLAY_ID} .jvr-close{font-size:20px;padding:4px 12px}
      #${OVERLAY_ID} .jvr-status{position:absolute;left:50%;bottom:20px;z-index:5;transform:translateX(-50%);padding:8px 12px;border-radius:7px;background:#111e;color:#ccd3da;font-size:13px;white-space:nowrap;pointer-events:none}
      @media(max-width:780px){#${OVERLAY_ID} .jvr-bar{flex-wrap:wrap}.jvr-label{display:none}}
    `;
    document.head.appendChild(style);
  }

  function createButton(label, action, value) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = label;
    button.dataset.action = action;
    if (value) button.dataset.value = value;
    return button;
  }

  function createSelect(id, action, options, label) {
    const select = document.createElement('select');
    select.id = id;
    select.dataset.action = action;
    options.forEach((option) => {
      const element = document.createElement('option');
      element.value = option.id;
      element.textContent = label(option);
      select.append(element);
    });
    return select;
  }

  function buildOverlay() {
    addStyles();
    overlay = document.createElement('div');
    overlay.id = OVERLAY_ID;
    const bar = document.createElement('div');
    bar.className = 'jvr-bar';
    const close = createButton('←', 'exit');
    close.className = 'jvr-close';
    bar.append(close);
    const version = document.createElement('span');
    version.className = 'jvr-label';
    version.textContent = 'Jellyfin VR v4.2';
    bar.append(version);
    [['360', '360'], ['180 Equirect', '180'], ['180 Fisheye', 'fisheye'], ['Cinema', 'flat']]
      .forEach(([label, value]) => bar.append(createButton(label, 'projection', value)));
    [['Mono', 'mono'], ['SBS', 'sbs'], ['OU', 'ou']]
      .forEach(([label, value]) => bar.append(createButton(label, 'stereo', value)));
    [['Void', 'void'], ['Theater', 'theater']]
      .forEach(([label, value]) => bar.append(createButton(label, 'environment', value)));
    bar.append(createSelect('jvr-lights-select', 'lights', LIGHT_LEVELS, (level) => `Lights: ${level.label}`));
    bar.append(createSelect('jvr-seat-select', 'seat', SEATS, (seat) => `Seat: ${seat.label}`));
    bar.append(createButton('Swap Eyes', 'swap'));
    const source = createButton('Original Source', 'source');
    source.id = 'jvr-source-toggle';
    bar.append(source);
    const quality = createSelect('jvr-quality-select', 'quality', QUALITY_PRESETS, (preset) => `Quality: ${preset.label}`);
    QUALITY_PRESETS.forEach((preset, index) => { quality.options[index].title = preset.note; });
    bar.append(quality);
    const enter = createButton('Enter VR', 'enter');
    enter.className = 'jvr-enter';
    bar.append(enter);
    statusEl = document.createElement('div');
    statusEl.className = 'jvr-status';
    statusEl.textContent = 'Initialising the native WebXR renderer...';
    overlay.append(bar, statusEl);
    document.body.appendChild(overlay);
    bar.addEventListener('click', handleToolbar);
    bar.addEventListener('change', handleToolbarChange);
  }

  function createPanel() {
    panelCanvas = document.createElement('canvas');
    panelCanvas.width = 1600;
    panelCanvas.height = 720;
    panelContext = panelCanvas.getContext('2d');
    panelTexture = new THREE.CanvasTexture(panelCanvas);
    panelTexture.colorSpace = THREE.SRGBColorSpace;
    const geometry = new THREE.PlaneGeometry(PANEL_WIDTH, PANEL_HEIGHT);
    const material = new THREE.MeshBasicMaterial({
      map: panelTexture,
      transparent: true,
      depthTest: false,
      depthWrite: false,
      side: THREE.DoubleSide,
      fog: false
    });
    panelMesh = new THREE.Mesh(geometry, material);
    panelMesh.name = 'jvr-control-panel';
    panelMesh.position.set(PANEL_HOME.x, PANEL_HOME.y, PANEL_HOME.z);
    panelMesh.renderOrder = 10000;
    panelMesh.visible = false;
    camera.add(panelMesh);

    const borderGeometry = new THREE.EdgesGeometry(geometry);
    const borderMaterial = new THREE.LineBasicMaterial({ color: 0x35cfff, transparent: true, opacity: 0.95, depthTest: false });
    panelBorder = new THREE.LineSegments(borderGeometry, borderMaterial);
    panelBorder.name = 'jvr-panel-drag-border';
    panelBorder.position.z = 0.006;
    panelBorder.renderOrder = 10002;
    panelMesh.add(panelBorder);

    const cursorGeometry = new THREE.RingGeometry(0.013, 0.023, 32);
    const cursorMaterial = new THREE.MeshBasicMaterial({ color: 0xffffff, side: THREE.DoubleSide, depthTest: false, depthWrite: false });
    panelCursor = new THREE.Mesh(cursorGeometry, cursorMaterial);
    panelCursor.name = 'jvr-panel-cursor';
    panelCursor.position.z = 0.012;
    panelCursor.renderOrder = 10003;
    panelCursor.visible = false;
    const cursorDot = new THREE.Mesh(
      new THREE.CircleGeometry(0.008, 24),
      new THREE.MeshBasicMaterial({ color: 0xffffff, side: THREE.DoubleSide, depthTest: false, depthWrite: false })
    );
    cursorDot.position.z = 0.001;
    panelCursor.add(cursorDot);
    panelMesh.add(panelCursor);
    drawPanel();
  }

  function roundedRect(context, x, y, width, height, radius) {
    context.beginPath();
    if (typeof context.roundRect === 'function') {
      context.roundRect(x, y, width, height, radius);
      return;
    }
    const r = Math.min(radius, width / 2, height / 2);
    context.moveTo(x + r, y);
    context.lineTo(x + width - r, y);
    context.quadraticCurveTo(x + width, y, x + width, y + r);
    context.lineTo(x + width, y + height - r);
    context.quadraticCurveTo(x + width, y + height, x + width - r, y + height);
    context.lineTo(x + r, y + height);
    context.quadraticCurveTo(x, y + height, x, y + height - r);
    context.lineTo(x, y + r);
    context.quadraticCurveTo(x, y, x + r, y);
    context.closePath();
  }

  function addPanelButton(x, y, width, height, label, action, color = '#27303a') {
    const context = panelContext;
    const hovered = hoverAction === action;
    roundedRect(context, x, y, width, height, 18);
    context.fillStyle = hovered ? '#008fc2' : color;
    context.fill();
    context.strokeStyle = hovered ? '#83ddff' : '#ffffff30';
    context.lineWidth = hovered ? 5 : 2;
    context.stroke();
    context.fillStyle = '#fff';
    context.textAlign = 'center';
    context.textBaseline = 'middle';
    fillFittedText(context, label, x + width / 2, y + height / 2, width - 28, 44, 24);
    panelButtons.push({ x, y, width, height, action });
  }

  // Lay out one row of buttons across the panel's full button span. `weight`
  // widens a button relative to its neighbours; everything else divides evenly.
  function addPanelRow(rowIndex, specs) {
    const gaps = PANEL_ROW.gap * (specs.length - 1);
    const total = specs.reduce((sum, spec) => sum + (spec.weight || 1), 0);
    const unit = (PANEL_ROW.width - gaps) / total;
    let x = PANEL_ROW.left;
    for (const spec of specs) {
      const width = unit * (spec.weight || 1);
      addPanelButton(x, PANEL_ROW.y[rowIndex], width, PANEL_ROW.height, spec.label, spec.action, spec.color);
      x += width + PANEL_ROW.gap;
    }
  }

  function fillFittedText(context, text, centerX, centerY, maxWidth, startSize, minSize = 26) {
    let size = startSize;
    do {
      context.font = `${size}px system-ui, sans-serif`;
      if (context.measureText(text).width <= maxWidth) break;
      size -= 2;
    } while (size > minSize);
    let output = text;
    if (context.measureText(output).width > maxWidth) {
      while (output.length > 4 && context.measureText(`${output}…`).width > maxWidth) output = output.slice(0, -1);
      output += '…';
    }
    context.fillText(output, centerX, centerY);
  }

  function drawPanel() {
    if (!panelContext || !panelTexture) return;
    const context = panelContext;
    panelButtons = [];
    context.clearRect(0, 0, panelCanvas.width, panelCanvas.height);
    roundedRect(context, 0, 0, 1600, 720, 30);
    context.fillStyle = '#10151bf7';
    context.fill();
    context.strokeStyle = '#36d2ffff';
    context.lineWidth = 12;
    context.stroke();
    context.fillStyle = '#123848';
    context.fillRect(28, 18, 1544, 16);
    context.fillStyle = '#5edcff';
    context.font = '28px system-ui, sans-serif';
    context.textAlign = 'right';
    context.fillText('Hold the border to move this panel', 1530, 52);
    context.fillStyle = '#fff';
    context.textAlign = 'center';
    context.textBaseline = 'middle';
    const title = (document.title || 'Jellyfin VR').replace(/\s+/g, ' ');
    fillFittedText(context, title, 800, 76, 1440, 52, 30);
    context.fillStyle = '#aeb9c4';
    context.font = '42px system-ui, sans-serif';
    const shownTime = dragState.type === 'seek' ? dragState.seekTime : getCurrentTime();
    context.fillText(`${formatTime(shownTime)} / ${formatTime(getDuration())}`, 800, 125);
    const duration = getDuration();
    const progress = duration > 0 ? THREE.MathUtils.clamp(shownTime / duration, 0, 1) : 0;
    roundedRect(context, TIMELINE.x, TIMELINE.trackY, TIMELINE.width, TIMELINE.trackHeight, 10);
    context.fillStyle = '#34414c';
    context.fill();
    roundedRect(context, TIMELINE.x, TIMELINE.trackY, Math.max(20, TIMELINE.width * progress), TIMELINE.trackHeight, 10);
    context.fillStyle = '#f5f8fb';
    context.fill();
    context.beginPath();
    context.arc(TIMELINE.x + TIMELINE.width * progress, TIMELINE.trackY + TIMELINE.trackHeight / 2, 24, 0, Math.PI * 2);
    context.fillStyle = '#fff';
    context.fill();
    context.strokeStyle = '#171d22';
    context.lineWidth = 5;
    context.stroke();
    addPanelRow(0, [
      { label: '-10', action: 'back' },
      { label: activeVideo?.paused ? 'PLAY' : 'PAUSE', action: 'play', weight: 1.4, color: '#006f96' },
      { label: '+10', action: 'forward' },
      { label: activeVideo?.muted ? 'MUTED' : 'SOUND', action: 'mute' },
      { label: currentMode.projection.toUpperCase(), action: 'projection' },
      { label: currentMode.stereo.toUpperCase(), action: 'stereo' }
    ]);
    // Lights and seat only mean anything inside a room; off it they stay on the
    // panel, greyed, rather than moving the buttons around under the viewer.
    const roomColor = environmentActive() ? '#2d4a40' : '#1e242b';
    addPanelRow(1, [
      { label: 'SWAP EYES', action: 'swap' },
      { label: environmentName.toUpperCase(), action: 'environment', color: '#2d4a40' },
      { label: `LIGHTS ${currentLightLevel().label}`, action: 'lights', color: roomColor },
      { label: `SEAT ${currentSeat().label}`, action: 'seat', color: roomColor },
      { label: sourceMode === 'compat' ? 'H264' : 'SOURCE', action: 'source', color: '#25536a' },
      { label: currentQuality().label, action: 'quality', color: '#25536a' }
    ]);
    addPanelRow(2, [
      { label: 'RECENTER', action: 'recenter', color: '#365061' },
      { label: 'RESET ALL', action: 'reset', color: '#365061' },
      { label: 'HIDE PANEL', action: 'hide' },
      { label: 'EXIT VR', action: 'exit-vr', color: '#652d34' }
    ]);
    context.fillStyle = '#81909f';
    fillFittedText(
      context,
      'Timeline: hold to scrub  ·  Trigger-drag to aim the view  ·  Thumbstick click to recentre (look up to lie back)',
      800,
      652,
      1480,
      34,
      22
    );
    panelTexture.needsUpdate = true;
  }

  function buildRenderer() {
    world = new THREE.Scene();
    world.background = new THREE.Color(0x000000);
    camera = new THREE.PerspectiveCamera(75, innerWidth / innerHeight, 0.01, 120);
    camera.position.set(0, EYE_HEIGHT, 0);
    camera.layers.enable(0);
    camera.layers.enable(1);
    world.add(camera);
    videoRoot = new THREE.Group();
    videoRoot.name = 'jvr-video-view-root';
    videoRoot.position.set(0, EYE_HEIGHT, 0);
    world.add(videoRoot);
    viewBase = null;
    viewYaw = 0;
    viewPitch = 0;
    // Everything the viewer can walk away from hangs off seatRoot: moving seats
    // means moving the room and the screen together, past a viewer whose pose
    // the XR runtime owns. videoRoot keeps its origin on the head, so aiming the
    // view still spins the room around the viewer rather than around row 0.
    seatRoot = new THREE.Group();
    seatRoot.name = 'jvr-seat-root';
    videoRoot.add(seatRoot);
    environmentRoot = new THREE.Group();
    environmentRoot.name = 'jvr-environment-root';
    environmentRoot.position.set(0, -EYE_HEIGHT, 0);
    environmentRoot.visible = false;
    environmentBuilt = '';
    seatRoot.add(environmentRoot);
    renderer = new THREE.WebGLRenderer({ antialias: true, alpha: false, powerPreference: 'high-performance' });
    renderer.outputColorSpace = THREE.SRGBColorSpace;
    renderer.setPixelRatio(Math.min(devicePixelRatio || 1, 2));
    renderer.setSize(innerWidth, innerHeight);
    renderer.xr.enabled = true;
    renderer.xr.setReferenceSpaceType('local-floor');
    renderer.domElement.className = 'jvr-canvas';
    overlay.prepend(renderer.domElement);
    createPanel();
    setupControllers();
    resizeHandler = () => {
      if (!renderer || !camera) return;
      camera.aspect = innerWidth / innerHeight;
      camera.updateProjectionMatrix();
      renderer.setSize(innerWidth, innerHeight);
    };
    window.addEventListener('resize', resizeHandler);
    renderer.setAnimationLoop(renderFrame);
  }

  function readStoredEnvironment() {
    try {
      const value = localStorage.getItem(ENV_STORE_KEY);
      return ENVIRONMENTS.includes(value) ? value : 'theater';
    } catch (_) {
      return 'theater';
    }
  }

  function rememberEnvironment() {
    try { localStorage.setItem(ENV_STORE_KEY, environmentName); } catch (_) { }
  }

  // An environment is only meaningful behind a flat screen: 180/360/fisheye wrap
  // the viewer in a radius-50 sphere that would swallow any room geometry.
  function environmentActive() {
    return environmentName !== 'void' && currentMode.projection === 'flat';
  }

  // The display aspect of one eye, which is what the screen has to be shaped
  // like. Pixel dimensions alone cannot answer this: in *half*-SBS/OU each eye
  // is squeezed back into an ordinary frame, so the eye's pixels are anamorphic
  // and the container aspect is already the display aspect; in *full*-SBS/OU the
  // frame is genuinely twice as wide (or tall) and has to be halved. The two are
  // told apart by which reading lands in the normal range of display ratios.
  function videoAspect() {
    const width = activeVideo?.videoWidth || sourceVideo?.videoWidth || 0;
    const height = activeVideo?.videoHeight || sourceVideo?.videoHeight || 0;
    if (!width || !height) return 16 / 9;
    const frame = width / height;
    if (!Number.isFinite(frame) || frame <= 0) return 16 / 9;
    const candidates = currentMode.stereo === 'sbs'
      ? [frame, frame / 2]
      : currentMode.stereo === 'ou'
        ? [frame, frame * 2]
        : [frame];
    // Half packing is listed first, so it wins a tie such as an SBS frame at
    // 2.4:1 (half-SBS scope, far more common than full-SBS of 1.2:1 content).
    const plausible = candidates.find((value) => value >= 1.2 && value <= 2.7);
    return THREE.MathUtils.clamp(plausible || candidates[0], 1, 3.2);
  }

  function computeScreenLayout() {
    const preset = SCREEN_LAYOUTS[environmentActive() ? environmentName : 'void'];
    const aspect = videoAspect();
    let height = preset.height;
    let width = height * aspect;
    if (width > preset.maxWidth) {
      width = preset.maxWidth;
      height = width / aspect;
    }
    return {
      width,
      height,
      y: preset.centerY,
      z: -preset.distance,
      radius: preset.curveRatio ? preset.distance * preset.curveRatio : 0
    };
  }

  function disposeMaterial(material) {
    if (!material) return;
    // Material.dispose() does not release the textures it references, and the
    // room's textures are canvases we generated, so they have to go too. The
    // video texture is shared and owned by closePlayer.
    for (const key of ['map', 'alphaMap', 'emissiveMap']) {
      if (material[key] && material[key] !== videoTexture) material[key].dispose();
    }
    material.dispose();
  }

  function disposeTree(root) {
    root?.traverse?.((object) => {
      if (object.isInstancedMesh) object.dispose();
      object.geometry?.dispose?.();
      if (Array.isArray(object.material)) object.material.forEach(disposeMaterial);
      else disposeMaterial(object.material);
    });
  }

  // --- procedural textures -------------------------------------------------
  // Everything is painted into a canvas at build time. The script has to stay a
  // single pasteable file, so it cannot ship image assets, and a texture is what
  // separates "grey boxes" from a room.

  function makeTexture(width, height, draw, repeatX = 1, repeatY = 1) {
    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    draw(canvas.getContext('2d'), width, height);
    const texture = new THREE.CanvasTexture(canvas);
    texture.colorSpace = THREE.SRGBColorSpace;
    texture.wrapS = THREE.RepeatWrapping;
    texture.wrapT = THREE.RepeatWrapping;
    texture.repeat.set(repeatX, repeatY);
    texture.anisotropy = Math.min(4, renderer?.capabilities?.getMaxAnisotropy?.() || 1);
    return texture;
  }

  function carpetTexture(repeat) {
    return makeTexture(128, 128, (context, size) => {
      context.fillStyle = '#412534';
      context.fillRect(0, 0, size, size);
      for (let i = 0; i < 2800; i += 1) {
        const shade = 34 + Math.random() * 62;
        context.fillStyle = `rgba(${Math.round(shade * 1.7)},${Math.round(shade * 0.8)},${Math.round(shade)},0.55)`;
        context.fillRect(Math.random() * size, Math.random() * size, 2, 2);
      }
      context.strokeStyle = 'rgba(196,96,120,0.26)';
      context.lineWidth = 3;
      for (const [cx, cy] of [[0, 0], [size, 0], [0, size], [size, size], [size / 2, size / 2]]) {
        context.beginPath();
        context.moveTo(cx, cy - 27);
        context.lineTo(cx + 27, cy);
        context.lineTo(cx, cy + 27);
        context.lineTo(cx - 27, cy);
        context.closePath();
        context.stroke();
      }
    }, repeat, repeat);
  }

  // Fabric-wrapped acoustic battens. The vertical zoning (skirting, field,
  // upper band) is baked down the canvas and mapped once over the wall height,
  // so only the horizontal rib rhythm repeats.
  function wallPanelTexture(repeatX) {
    return makeTexture(128, 512, (context, width, height) => {
      const gradient = context.createLinearGradient(0, 0, 0, height);
      gradient.addColorStop(0, '#262930');
      gradient.addColorStop(0.16, '#3f4149');
      gradient.addColorStop(0.78, '#3a3c44');
      gradient.addColorStop(0.86, '#24262b');
      gradient.addColorStop(1, '#1a1c20');
      context.fillStyle = gradient;
      context.fillRect(0, 0, width, height);
      for (let rib = 0; rib < 4; rib += 1) {
        const x = rib * 32;
        context.fillStyle = 'rgba(0,0,0,0.45)';
        context.fillRect(x, 0, 3, height);
        context.fillStyle = 'rgba(255,255,255,0.055)';
        context.fillRect(x + 3, 0, 2, height);
      }
      for (let i = 0; i < 6000; i += 1) {
        context.fillStyle = `rgba(255,255,255,${Math.random() * 0.04})`;
        context.fillRect(Math.random() * width, Math.random() * height, 1, 1);
      }
    }, repeatX, 1);
  }

  function curtainTexture(repeatX) {
    return makeTexture(128, 256, (context, width, height) => {
      for (let x = 0; x < width; x += 1) {
        const fold = 0.5 + 0.5 * Math.cos((x / width) * Math.PI * 2 * 4);
        const shade = 16 + fold * 58;
        context.fillStyle = `rgb(${Math.round(shade * 2)},${Math.round(shade * 0.4)},${Math.round(shade * 0.6)})`;
        context.fillRect(x, 0, 1, height);
      }
      const gradient = context.createLinearGradient(0, 0, 0, height);
      gradient.addColorStop(0, 'rgba(0,0,0,0.35)');
      gradient.addColorStop(0.35, 'rgba(0,0,0,0)');
      gradient.addColorStop(1, 'rgba(0,0,0,0.6)');
      context.fillStyle = gradient;
      context.fillRect(0, 0, width, height);
    }, repeatX, 1);
  }

  function glowTexture() {
    return makeTexture(128, 128, (context, size) => {
      const gradient = context.createRadialGradient(size / 2, size / 2, 0, size / 2, size / 2, size / 2);
      gradient.addColorStop(0, 'rgba(255,255,255,1)');
      gradient.addColorStop(0.3, 'rgba(255,255,255,0.4)');
      gradient.addColorStop(1, 'rgba(255,255,255,0)');
      context.fillStyle = gradient;
      context.fillRect(0, 0, size, size);
    });
  }

  // Soft rectangular falloff for the light the screen throws onto the front
  // wall. A blurred rounded rect reads far better than a hard additive quad.
  function bleedTexture(inset) {
    return makeTexture(256, 256, (context, size) => {
      context.clearRect(0, 0, size, size);
      context.filter = 'blur(24px)';
      context.fillStyle = 'rgba(255,255,255,0.9)';
      roundedRect(context, size * inset, size * inset, size * (1 - inset * 2), size * (1 - inset * 2), 24);
      context.fill();
      context.filter = 'none';
    });
  }

  function exitSignTexture() {
    return makeTexture(128, 64, (context, width, height) => {
      context.fillStyle = '#041008';
      context.fillRect(0, 0, width, height);
      context.fillStyle = '#1cff78';
      context.fillRect(5, 5, width - 10, height - 10);
      context.fillStyle = '#04210e';
      context.font = 'bold 36px system-ui, sans-serif';
      context.textAlign = 'center';
      context.textBaseline = 'middle';
      context.fillText('EXIT', width / 2, height / 2 + 2);
    });
  }

  // --- geometry helpers ----------------------------------------------------

  function paintGeometry(geometry, color) {
    const count = geometry.attributes.position.count;
    const array = new Float32Array(count * 3);
    const value = new THREE.Color(color);
    for (let index = 0; index < count; index += 1) {
      array[index * 3] = value.r;
      array[index * 3 + 1] = value.g;
      array[index * 3 + 2] = value.b;
    }
    geometry.setAttribute('color', new THREE.BufferAttribute(array, 3));
    return geometry;
  }

  // Three's BufferGeometryUtils lives under examples/, which a single pasteable
  // file cannot import, so parts are merged by hand. Converting to non-indexed
  // first reduces the merge to concatenating attribute arrays.
  function mergeParts(parts) {
    const expanded = parts.map((geometry) => (geometry.index ? geometry.toNonIndexed() : geometry));
    const keys = ['position', 'normal', 'uv', 'color'].filter(
      (key) => expanded.every((geometry) => geometry.attributes[key])
    );
    const merged = new THREE.BufferGeometry();
    for (const key of keys) {
      const itemSize = expanded[0].attributes[key].itemSize;
      let total = 0;
      for (const geometry of expanded) total += geometry.attributes[key].count * itemSize;
      const array = new Float32Array(total);
      let offset = 0;
      for (const geometry of expanded) {
        array.set(geometry.attributes[key].array, offset);
        offset += geometry.attributes[key].count * itemSize;
      }
      merged.setAttribute(key, new THREE.BufferAttribute(array, itemSize));
    }
    expanded.forEach((geometry, index) => { if (geometry !== parts[index]) geometry.dispose(); });
    parts.forEach((geometry) => geometry.dispose());
    return merged;
  }

  // Bend a plane around a vertical cylinder of the given radius, keeping arc
  // length (so the screen stays `width` metres of picture) and leaving UVs
  // untouched, which is what lets the stereo UV baking run afterwards.
  function bendAroundY(geometry, radius) {
    if (!radius) return geometry;
    const position = geometry.attributes.position;
    for (let index = 0; index < position.count; index += 1) {
      const theta = position.getX(index) / radius;
      position.setXYZ(
        index,
        radius * Math.sin(theta),
        position.getY(index),
        position.getZ(index) + radius * (1 - Math.cos(theta))
      );
    }
    position.needsUpdate = true;
    geometry.computeVertexNormals();
    return geometry;
  }

  // Box projection: pick the UV plane per vertex from the dominant normal axis,
  // so one tiling scale holds across treads, risers and walls alike.
  function boxProjectUV(geometry, scale) {
    const position = geometry.attributes.position;
    const normal = geometry.attributes.normal;
    const uv = new Float32Array(position.count * 2);
    for (let index = 0; index < position.count; index += 1) {
      const nx = Math.abs(normal.getX(index));
      const ny = Math.abs(normal.getY(index));
      const nz = Math.abs(normal.getZ(index));
      let u;
      let v;
      if (ny >= nx && ny >= nz) { u = position.getX(index); v = position.getZ(index); }
      else if (nx >= nz) { u = position.getZ(index); v = position.getY(index); }
      else { u = position.getX(index); v = position.getY(index); }
      uv[index * 2] = u / scale;
      uv[index * 2 + 1] = v / scale;
    }
    geometry.setAttribute('uv', new THREE.BufferAttribute(uv, 2));
    return geometry;
  }

  function plane(width, height, position, rotation, segments = 1) {
    const geometry = new THREE.PlaneGeometry(width, height, segments, 1);
    if (rotation) {
      if (rotation[0]) geometry.rotateX(rotation[0]);
      if (rotation[1]) geometry.rotateY(rotation[1]);
    }
    geometry.translate(position[0], position[1], position[2]);
    return geometry;
  }

  function box(width, height, depth, position, rotation) {
    const geometry = new THREE.BoxGeometry(width, height, depth);
    if (rotation) geometry.rotateX(rotation);
    geometry.translate(position[0], position[1], position[2]);
    return geometry;
  }

  function rowLevel(row) {
    return { y: row * THEATER.riser, z: row * THEATER.rowDepth };
  }

  function floorExtent() {
    const front = rowLevel(-THEATER.rowsFront);
    const back = rowLevel(THEATER.rowsBack);
    return {
      lowY: front.y - THEATER.riser,
      lowEdgeZ: front.z - THEATER.rowDepth / 2,
      highY: back.y + THEATER.riser,
      highEdgeZ: back.z + THEATER.rowDepth / 2
    };
  }

  // --- auditorium ----------------------------------------------------------

  // One cinema seat, origin at the floor between its feet, facing -z. Five boxes
  // merged into a single vertex-coloured geometry so the whole house is one
  // instanced draw call rather than one per seat part.
  function makeSeatGeometry() {
    const FRAME = 0x2b2733;
    const FABRIC = 0x73202f;
    return mergeParts([
      paintGeometry(box(0.5, 0.4, 0.46, [0, 0.2, 0.02]), FRAME),
      paintGeometry(box(0.5, 0.13, 0.46, [0, 0.455, 0], 0.09), FABRIC),
      paintGeometry(box(0.52, 0.78, 0.14, [0, 0.88, 0.22], 0.2), FABRIC),
      paintGeometry(box(0.08, 0.09, 0.44, [-0.29, 0.6, 0.04]), FRAME),
      paintGeometry(box(0.08, 0.09, 0.44, [0.29, 0.6, 0.04]), FRAME)
    ]);
  }

  function seatPlacements(occupiedRow) {
    const placements = [];
    const count = Math.floor((THEATER.seatHalfSpan * 2) / THEATER.seatPitch);
    for (let row = -THEATER.rowsFront; row <= THEATER.rowsBack; row += 1) {
      const level = rowLevel(row);
      const radius = level.z - THEATER.arcCenterZ;
      for (let seat = 0; seat < count; seat += 1) {
        const arc = (seat - (count - 1) / 2) * THEATER.seatPitch;
        const distance = Math.abs(arc);
        if (distance >= THEATER.aisleInner && distance <= THEATER.aisleOuter) continue;
        // Leave the viewer's own spot clear; they are sitting in it.
        if (row === occupiedRow && distance < THEATER.standingGap) continue;
        const angle = arc / radius;
        placements.push({
          x: radius * Math.sin(angle),
          y: level.y,
          z: THEATER.arcCenterZ + radius * Math.cos(angle),
          angle
        });
      }
    }
    return placements;
  }

  function buildSeating(group) {
    const placements = seatPlacements(seatRow());
    const seats = new THREE.InstancedMesh(
      makeSeatGeometry(),
      new THREE.MeshLambertMaterial({ vertexColors: true }),
      placements.length
    );
    const matrix = new THREE.Matrix4();
    const position = new THREE.Vector3();
    const quaternion = new THREE.Quaternion();
    const scale = new THREE.Vector3(1, 1, 1);
    const axis = new THREE.Vector3(0, 1, 0);
    placements.forEach((placement, index) => {
      position.set(placement.x, placement.y, placement.z);
      quaternion.setFromAxisAngle(axis, placement.angle);
      seats.setMatrixAt(index, matrix.compose(position, quaternion, scale));
    });
    seats.instanceMatrix.needsUpdate = true;
    // The bounding sphere of a single seat would cull the whole house.
    seats.name = 'jvr-theater-seats';
    seats.userData.jvrSeatRow = seatRow();
    seats.frustumCulled = false;
    seats.layers.set(0);
    group.add(seats);
  }

  function buildFloor(group) {
    const width = THEATER.wallX * 2;
    const extent = floorExtent();
    const parts = [];
    for (let row = -THEATER.rowsFront; row <= THEATER.rowsBack; row += 1) {
      const level = rowLevel(row);
      parts.push(plane(width, THEATER.rowDepth, [0, level.y, level.z], [-Math.PI / 2, 0]));
      // The riser at the screen-facing edge of a tread is only ever seen from
      // further down the rake, so it faces -z.
      parts.push(plane(width, THEATER.riser, [0, level.y - THEATER.riser / 2, level.z - THEATER.rowDepth / 2], [0, Math.PI]));
    }
    const frontDepth = extent.lowEdgeZ - THEATER.screenWallZ;
    parts.push(plane(width, frontDepth, [0, extent.lowY, (extent.lowEdgeZ + THEATER.screenWallZ) / 2], [-Math.PI / 2, 0]));
    const backDepth = THEATER.backWallZ - extent.highEdgeZ;
    parts.push(plane(width, backDepth, [0, extent.highY, (extent.highEdgeZ + THEATER.backWallZ) / 2], [-Math.PI / 2, 0]));
    parts.push(plane(width, THEATER.riser, [0, extent.highY - THEATER.riser / 2, extent.highEdgeZ], [0, Math.PI]));

    const floor = new THREE.Mesh(
      boxProjectUV(mergeParts(parts), 2),
      new THREE.MeshLambertMaterial({ map: carpetTexture(1) })
    );
    floor.name = 'jvr-theater-floor';
    floor.layers.set(0);
    group.add(floor);
  }

  function buildShell(group) {
    const extent = floorExtent();
    const length = THEATER.backWallZ - THEATER.screenWallZ;
    const height = THEATER.ceilingY - extent.lowY;
    const midY = (THEATER.ceilingY + extent.lowY) / 2;
    const midZ = (THEATER.backWallZ + THEATER.screenWallZ) / 2;

    const sideMaterial = new THREE.MeshLambertMaterial({ map: wallPanelTexture(length / 2) });
    for (const side of [-1, 1]) {
      const wall = new THREE.Mesh(new THREE.PlaneGeometry(length, height), sideMaterial);
      wall.position.set(side * THEATER.wallX, midY, midZ);
      wall.rotation.y = side < 0 ? Math.PI / 2 : -Math.PI / 2;
      wall.name = `jvr-theater-wall-${side < 0 ? 'left' : 'right'}`;
      wall.layers.set(0);
      group.add(wall);
    }

    const back = new THREE.Mesh(
      new THREE.PlaneGeometry(THEATER.wallX * 2, height),
      new THREE.MeshLambertMaterial({ map: wallPanelTexture(THEATER.wallX) })
    );
    back.position.set(0, midY, THEATER.backWallZ);
    back.rotation.y = Math.PI;
    back.name = 'jvr-theater-wall-back';
    back.layers.set(0);
    group.add(back);

    // The wall the screen hangs on stays matte near-black so nothing competes
    // with the picture.
    const front = new THREE.Mesh(
      new THREE.PlaneGeometry(THEATER.wallX * 2, height),
      new THREE.MeshLambertMaterial({ color: 0x101217 })
    );
    front.position.set(0, midY, THEATER.screenWallZ);
    front.name = 'jvr-theater-wall-front';
    front.layers.set(0);
    group.add(front);

    const ceilingParts = [plane(THEATER.wallX * 2, length, [0, THEATER.ceilingY, midZ], [Math.PI / 2, 0])];
    for (let beam = 0; beam < 6; beam += 1) {
      const z = THEATER.screenWallZ + 2 + beam * ((length - 4) / 5);
      ceilingParts.push(box(THEATER.wallX * 2, 0.3, 0.55, [0, THEATER.ceilingY - 0.15, z]));
    }
    const ceiling = new THREE.Mesh(
      mergeParts(ceilingParts),
      new THREE.MeshLambertMaterial({ color: 0x1b1e23 })
    );
    ceiling.name = 'jvr-theater-ceiling';
    ceiling.layers.set(0);
    group.add(ceiling);

    // Stage apron under the screen, with the two screen-channel speaker stacks.
    const stageTop = extent.lowY + THEATER.stageHeight;
    const stageParts = [
      plane(THEATER.wallX * 2, 2.4, [0, stageTop, THEATER.screenWallZ + 1.2], [-Math.PI / 2, 0]),
      plane(THEATER.wallX * 2, THEATER.stageHeight, [0, stageTop - THEATER.stageHeight / 2, THEATER.screenWallZ + 2.4], [0, 0])
    ];
    // for (const side of [-1, 1]) {
    //   stageParts.push(box(0.75, 1.9, 0.6, [side * 6.6, stageTop + 0.95, THEATER.screenWallZ + 0.9]));
    // }
    const stage = new THREE.Mesh(mergeParts(stageParts), new THREE.MeshLambertMaterial({ color: 0x16181d }));
    stage.name = 'jvr-theater-stage';
    stage.layers.set(0);
    group.add(stage);
  }

  function buildFixtures(group) {
    const extent = floorExtent();
    const glow = glowTexture();
    const glowMaterial = new THREE.MeshBasicMaterial({
      map: glow,
      color: 0xffb066,
      transparent: true,
      blending: THREE.AdditiveBlending,
      depthWrite: false
    });
    const lampParts = [];
    const housingParts = [];

    // Wall sconces down both side walls.
    for (let index = 0; index < 6; index += 1) {
      const z = THEATER.screenWallZ + 3 + index * 3.2;
      for (const side of [-1, 1]) {
        const x = side * (THEATER.wallX - 0.08);
        lampParts.push(plane(1.2, 1.8, [x, 2.2, z], [0, side < 0 ? Math.PI / 2 : -Math.PI / 2]));
        housingParts.push(box(0.12, 0.62, 0.18, [side * (THEATER.wallX - 0.09), 2.2, z]));
      }
    }
    // Aisle markers: a small glowing pad on the tread at each row end, which is
    // what stays visible from a seat. Riser-mounted lights would face away.
    for (let row = -THEATER.rowsFront; row <= THEATER.rowsBack; row += 1) {
      const level = rowLevel(row);
      for (const side of [-1, 1]) {
        for (const edge of [THEATER.aisleInner, THEATER.aisleOuter]) {
          lampParts.push(plane(0.34, 0.34, [side * edge, level.y + 0.012, level.z], [-Math.PI / 2, 0]));
        }
      }
    }
    const lamps = new THREE.Mesh(mergeParts(lampParts), glowMaterial);
    lamps.name = 'jvr-theater-lamps';
    lamps.userData.jvrLight = { role: 'sconce', base: 1 };
    lamps.layers.set(0);
    lamps.renderOrder = 2;
    group.add(lamps);
    const housings = new THREE.Mesh(mergeParts(housingParts), new THREE.MeshLambertMaterial({ color: 0x1c1f26 }));
    housings.name = 'jvr-theater-housings';
    housings.layers.set(0);
    group.add(housings);

    // Ceiling downlights, dim enough to read as fixtures rather than lamps.
    const downlights = new THREE.Mesh(
      mergeParts(
        [-4.5, 0, 4.5].flatMap((x) => [-9, -4.5, 0, 4.5].map(
          (z) => plane(0.9, 0.9, [x, THEATER.ceilingY - 0.02, z], [Math.PI / 2, 0])
        ))
      ),
      new THREE.MeshBasicMaterial({
        map: glow.clone(),
        color: 0xffc98a,
        transparent: true,
        opacity: 0.35,
        blending: THREE.AdditiveBlending,
        depthWrite: false
      })
    );
    downlights.material.map.needsUpdate = true;
    downlights.name = 'jvr-theater-downlights';
    downlights.userData.jvrLight = { role: 'downlight', base: 0.35 };
    downlights.layers.set(0);
    group.add(downlights);

    // Exit signs: two at the back of the house, two down by the screen. Nothing
    // says "cinema" faster than a green sign glowing in the dark.
    const signMaterial = new THREE.MeshBasicMaterial({ map: exitSignTexture(), transparent: true });
    const signs = [];
    for (const side of [-1, 1]) {
      signs.push(plane(0.86, 0.43, [side * (THEATER.wallX - 1.6), extent.highY + 2.3, THEATER.backWallZ - 0.06], [0, Math.PI]));
      signs.push(plane(0.86, 0.43, [side * (THEATER.wallX - 0.07), extent.lowY + 2.3, THEATER.screenWallZ + 3.4], [0, side < 0 ? Math.PI / 2 : -Math.PI / 2]));
    }
    const signMesh = new THREE.Mesh(mergeParts(signs), signMaterial);
    signMesh.name = 'jvr-theater-exit-signs';
    // A sign is a lit panel, not a glow: dimming it darkens the colour rather
    // than fading it into the wall.
    signMesh.userData.jvrLight = { role: 'exit', base: 1, tint: true };
    signMesh.layers.set(0);
    group.add(signMesh);

    // Projection booth ports on the back wall.
    const ports = new THREE.Mesh(
      mergeParts([-1.1, 1.1].map((x) => plane(1.1, 0.8, [x, extent.highY + 3.4, THEATER.backWallZ - 0.05], [0, Math.PI]))),
      new THREE.MeshBasicMaterial({ color: 0x0a1420 })
    );
    ports.name = 'jvr-theater-booth-ports';
    ports.layers.set(0);
    group.add(ports);
  }

  function buildTheaterLights(group) {
    const extent = floorExtent();
    const screenY = EYE_HEIGHT + SCREEN_LAYOUTS.theater.centerY;
    // Ambient only keeps the far corners off pure black: a real auditorium is
    // lit almost entirely by the screen, so the falloff has to come from the
    // point lights, not from a uniform fill.
    const ambient = new THREE.AmbientLight(0x35435c, 1);
    ambient.userData.jvrLight = { role: 'ambient', base: 1 };
    group.add(ambient);
    // decay 0 drops the inverse-square term and leaves a plain distance window,
    // which is far easier to tune than physical candela for a room this size.
    //
    // The window has to be short. With decay 0 the only falloff is the cutoff
    // term, (1 - (d/cutoff)^4)^2, which stays near 1 until d approaches the
    // cutoff: at 19 m this light still delivered 86% of full intensity to a
    // side wall 10 m away and half of it to the middle of the house, which is a
    // room flood, not a screen wash. Spill never rises above 1, so no house
    // setting could take it back down and the walls stayed lit. At 12 m
    // it lights the proscenium and the stage and is gone by mid-house, which is
    // how far a real screen throws.
    const key = new THREE.PointLight(0x9fc4ea, 3.4, 12, 0);
    key.position.set(0, screenY, THEATER.screenWallZ + 2.4);
    key.userData.jvrLight = { role: 'spill', base: 3.4 };
    group.add(key);
    // Bounce off the stage, so the front rows are not lit from nowhere.
    const bounce = new THREE.PointLight(0x7f9ec4, 1.2, 10, 0);
    bounce.position.set(0, extent.lowY + 1.4, THEATER.screenWallZ + 4.2);
    bounce.userData.jvrLight = { role: 'spill', base: 1.2 };
    group.add(bounce);
    const house = new THREE.PointLight(0xffa864, 1.4, 17, 0);
    house.position.set(0, extent.highY + 2.6, THEATER.backWallZ - 2);
    house.userData.jvrLight = { role: 'house', base: 1.4 };
    group.add(house);
  }

  // --- house lights and seating -------------------------------------------

  function currentLightLevel() {
    return LIGHT_LEVELS.find((level) => level.id === lightLevelId) || LIGHT_LEVELS[1];
  }

  function currentSeat() {
    return SEATS.find((seat) => seat.id === seatId) || SEATS[2];
  }

  // Off the flat screen there is no room to sit in, so the viewer stays on the
  // origin the 180/360 spheres are centred on.
  function seatRow() {
    return environmentActive() ? currentSeat().row : 0;
  }

  // Scales every tagged light and glow sprite in place. The room is a few
  // thousand merged triangles and rebuilding it would drop frames, so dimming
  // touches only intensities and opacities.
  function applyLightLevel() {
    const level = currentLightLevel();
    environmentRoot?.traverse((object) => {
      const tag = object.userData?.jvrLight;
      if (!tag) return;
      const scale = level[tag.role] ?? 1;
      if (object.isLight) object.intensity = tag.base * scale;
      else if (tag.tint) object.material.color.setScalar(Math.min(1, tag.base * scale));
      // Additive glow: opacity is the only knob, so it clamps rather than
      // letting a bright preset push it past full.
      else if (object.material) object.material.opacity = Math.min(1, tag.base * scale);
    });
    if (environmentFog) environmentFog.density = 0.016 * (level.fog ?? 1);
  }

  // The seat the viewer occupies has to be empty, which the house is built
  // with, so a move rebuilds the one instanced seat mesh and nothing else.
  function refreshSeating() {
    if (!environmentRoot || environmentBuilt !== 'theater') return;
    const existing = environmentRoot.getObjectByName('jvr-theater-seats');
    if (existing?.userData.jvrSeatRow === seatRow()) return;
    if (existing) {
      environmentRoot.remove(existing);
      disposeTree(existing);
    }
    buildSeating(environmentRoot);
  }

  function applySeat() {
    if (!seatRoot) return;
    const level = rowLevel(seatRow());
    // The viewer cannot be moved, so the room and the screen move past them:
    // the chosen row's tread lands on the real floor, and the screen gets
    // nearer or further by exactly the rows crossed.
    seatRoot.position.set(0, -level.y, -level.z);
    refreshSeating();
  }

  function readStoredChoice(key, options, fallback) {
    try {
      const value = localStorage.getItem(key);
      return options.some((option) => option.id === value) ? value : fallback;
    } catch (_) {
      return fallback;
    }
  }

  function applyLights(id, remember = true) {
    if (!LIGHT_LEVELS.some((level) => level.id === id)) return;
    lightLevelId = id;
    if (remember) { try { localStorage.setItem(LIGHT_STORE_KEY, lightLevelId); } catch (_) { } }
    applyLightLevel();
    updateToolbar();
    drawPanel();
  }

  function applySeatChoice(id, remember = true) {
    if (!SEATS.some((seat) => seat.id === id)) return;
    seatId = id;
    if (remember) { try { localStorage.setItem(SEAT_STORE_KEY, seatId); } catch (_) { } }
    applySeat();
    updateToolbar();
    drawPanel();
  }

  function cycleLights() {
    const index = LIGHT_LEVELS.findIndex((level) => level.id === lightLevelId);
    applyLights(LIGHT_LEVELS[(index + 1) % LIGHT_LEVELS.length].id);
  }

  function cycleSeat() {
    const index = SEATS.findIndex((seat) => seat.id === seatId);
    applySeatChoice(SEATS[(index + 1) % SEATS.length].id);
  }

  function buildTheater(group) {
    buildFloor(group);
    buildShell(group);
    buildSeating(group);
    buildFixtures(group);
    buildTheaterLights(group);
  }

  // Masking, drapes and the screen wash all track the screen, whose width
  // follows the video's aspect ratio, so they are rebuilt separately from the
  // room itself.
  function buildScreenSurround() {
    if (screenSurround) {
      screenSurround.parent?.remove(screenSurround);
      disposeTree(screenSurround);
      screenSurround = null;
    }
    if (!environmentRoot || !environmentActive()) return;
    const { width, height, y, z, radius } = screenLayout;
    const centerY = y + EYE_HEIGHT;
    const group = new THREE.Group();
    group.name = 'jvr-screen-surround';

    // Black velvet masking, curved to sit just behind the screen. A bend only
    // moves vertices, so a single-quad strip would cut the curve as a flat
    // chord and the picture would bulge a metre through its own frame.
    const border = 0.45;
    const arcSegments = (span) => (radius ? Math.max(1, Math.round(span / 0.4)) : 1);
    const mask = mergeParts([
      plane(width + border * 2, border, [0, centerY + height / 2 + border / 2, z - 0.06], null, arcSegments(width)),
      plane(width + border * 2, border, [0, centerY - height / 2 - border / 2, z - 0.06], null, arcSegments(width)),
      plane(border, height, [-width / 2 - border / 2, centerY, z - 0.06], null, arcSegments(border)),
      plane(border, height, [width / 2 + border / 2, centerY, z - 0.06], null, arcSegments(border))
    ]);
    const maskMesh = new THREE.Mesh(bendAroundY(mask, radius), new THREE.MeshBasicMaterial({ color: 0x040507 }));
    maskMesh.name = 'jvr-screen-mask';
    maskMesh.layers.set(0);
    group.add(maskMesh);

    // Pleated drapes flanking the masking, running down to the stage.
    const extent = floorExtent();
    const drapeTop = centerY + height / 2 + border;
    const drapeBottom = extent.lowY + THEATER.stageHeight;
    const drapeHeight = drapeTop - drapeBottom;
    const drapeWidth = Math.max(0.8, THEATER.wallX - 0.6 - (width / 2 + border));
    if (drapeHeight > 0.5) {
      const drapes = mergeParts([-1, 1].map((side) => plane(
        drapeWidth,
        drapeHeight,
        [side * (width / 2 + border + drapeWidth / 2), drapeBottom + drapeHeight / 2, z - 0.12],
        null,
        arcSegments(drapeWidth)
      )));
      const drapeMesh = new THREE.Mesh(
        bendAroundY(drapes, radius),
        new THREE.MeshLambertMaterial({ map: curtainTexture(Math.max(1, Math.round(drapeWidth * 1.6))) })
      );
      drapeMesh.name = 'jvr-screen-drapes';
      drapeMesh.layers.set(0);
      group.add(drapeMesh);
    }

    // Proscenium soffit above the masking.
    const soffitY = drapeTop + 0.55;
    if (soffitY + 0.5 < THEATER.ceilingY) {
      const soffit = new THREE.Mesh(
        box(THEATER.wallX * 2, 1.1, 0.9, [0, soffitY, z - 0.4]),
        new THREE.MeshLambertMaterial({ color: 0x131519 })
      );
      soffit.name = 'jvr-screen-soffit';
      soffit.layers.set(0);
      group.add(soffit);
    }

    // The wash the picture throws onto the wall around it. The halo is as wide
    // as the space actually left around the masking — walls, ceiling and stage
    // all crowd it, and a fixed multiple of the screen would punch through them.
    const outerHalfWidth = width / 2 + border;
    const outerHalfHeight = height / 2 + border;
    const halo = THREE.MathUtils.clamp(
      Math.min(
        THEATER.wallX - 0.3 - outerHalfWidth,
        THEATER.ceilingY - 0.25 - (centerY + outerHalfHeight),
        centerY - outerHalfHeight - (extent.lowY + 0.05)
      ),
      0.3,
      2
    );
    const bleed = new THREE.Mesh(
      bendAroundY(
        new THREE.PlaneGeometry((outerHalfWidth + halo) * 2, (outerHalfHeight + halo) * 2, radius ? 32 : 1, 1),
        radius
      ),
      new THREE.MeshBasicMaterial({
        map: bleedTexture(halo / ((outerHalfWidth + halo) * 2)),
        color: 0x5f9ec8,
        transparent: true,
        opacity: 0.36,
        blending: THREE.AdditiveBlending,
        depthWrite: false
      })
    );
    bleed.position.set(0, centerY, z - 0.2);
    bleed.name = 'jvr-screen-bleed';
    bleed.userData.jvrLight = { role: 'spill', base: 0.36 };
    bleed.layers.set(0);
    group.add(bleed);

    environmentRoot.add(group);
    screenSurround = group;
  }

  function buildEnvironment() {
    if (!environmentRoot) return;
    const wanted = environmentActive() ? environmentName : '';
    if (environmentBuilt === wanted) return;
    for (let index = environmentRoot.children.length - 1; index >= 0; index -= 1) {
      const child = environmentRoot.children[index];
      environmentRoot.remove(child);
      disposeTree(child);
    }
    screenSurround = null;
    environmentBuilt = wanted;
    if (wanted === 'theater') buildTheater(environmentRoot);
  }

  function syncEnvironment() {
    if (!environmentRoot) return;
    const active = environmentActive();
    buildEnvironment();
    buildScreenSurround();
    environmentRoot.visible = active;
    // Haze gives a big dark room its depth. It has to come off with the room:
    // the 180/360 sphere sits at radius 50 and would be fogged into a flat wash.
    // The screen and the panel opt out via `fog: false` on their own materials.
    if (active && !environmentFog) environmentFog = new THREE.FogExp2(0x05060a, 0.016);
    if (world) world.fog = active ? environmentFog : null;
    applyLightLevel();
    applySeat();
    // A room has a horizon; keep it level even if the view was pitched earlier.
    // A reclined recentre is not undone here: its tilt lives in viewBase.
    if (environmentActive()) {
      viewPitch = 0;
      applyViewRotation();
    }
  }

  function applyEnvironment(name, remember) {
    if (!ENVIRONMENTS.includes(name)) return;
    environmentName = name;
    if (remember) rememberEnvironment();
    // Asking for a room implies asking for the screen the room is built around.
    if (environmentName !== 'void' && currentMode.projection !== 'flat') {
      applyMode({ projection: 'flat' }, remember);
      return;
    }
    rebuildVideoMeshes();
  }

  function cycleEnvironment() {
    const next = ENVIRONMENTS[(ENVIRONMENTS.indexOf(environmentName) + 1) % ENVIRONMENTS.length];
    applyEnvironment(next, true);
  }

  function makeGeometry(projection, eye) {
    let geometry;
    if (projection === '360') {
      geometry = new THREE.SphereGeometry(50, 64, 32, 0, Math.PI * 2, 0, Math.PI);
      geometry.scale(-1, 1, 1);
    } else if (projection === 'flat') {
      geometry = new THREE.PlaneGeometry(screenLayout.width, screenLayout.height, screenLayout.radius ? 48 : 1, 1);
      bendAroundY(geometry, screenLayout.radius);
    } else {
      geometry = new THREE.SphereGeometry(50, 64, 32, -Math.PI / 2, Math.PI, 0, Math.PI);
      geometry.scale(-1, 1, 1);
    }
    if (currentMode.stereo !== 'mono') {
      const selectedEye = currentMode.swap ? 1 - eye : eye;
      const uv = geometry.attributes.uv;
      for (let index = 0; index < uv.count; index += 1) {
        let u = uv.getX(index);
        let v = uv.getY(index);
        if (currentMode.stereo === 'sbs') u = u * 0.5 + selectedEye * 0.5;
        else v = v * 0.5 + (selectedEye === 0 ? 0.5 : 0);
        uv.setXY(index, u, v);
      }
      uv.needsUpdate = true;
    }
    return geometry;
  }

  function makeMaterial(eye) {
    if (currentMode.projection !== 'fisheye') {
      return new THREE.MeshBasicMaterial({
        map: videoTexture,
        side: currentMode.projection === 'flat' ? THREE.DoubleSide : THREE.FrontSide,
        toneMapped: false,
        fog: false
      });
    }
    const selectedEye = currentMode.swap ? 1 - eye : eye;
    const stereoLine = currentMode.stereo === 'sbs'
      ? `p.x=p.x*0.5+${selectedEye ? '0.5' : '0.0'};`
      : currentMode.stereo === 'ou'
        ? `p.y=p.y*0.5+${selectedEye ? '0.0' : '0.5'};`
        : '';
    return new THREE.ShaderMaterial({
      side: THREE.FrontSide,
      uniforms: { map: { value: videoTexture } },
      vertexShader: `varying vec3 vLocal;void main(){vLocal=normalize(position);gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.0);}`,
      fragmentShader: `uniform sampler2D map;varying vec3 vLocal;void main(){vec2 p=vLocal.xy*0.5+0.5;${stereoLine}gl_FragColor=texture2D(map,p);}`
    });
  }

  function disposeVideoMeshes() {
    videoMeshes.forEach((mesh) => {
      mesh.parent?.remove(mesh);
      mesh.geometry?.dispose();
      mesh.material?.dispose();
    });
    videoMeshes = [];
  }

  function rebuildVideoMeshes() {
    if (!world) return;
    screenLayout = computeScreenLayout();
    syncEnvironment();
    if (videoTexture) {
      disposeVideoMeshes();
      const stereo = currentMode.stereo !== 'mono';
      const left = new THREE.Mesh(makeGeometry(currentMode.projection, 0), makeMaterial(0));
      left.name = 'jvr-left-video';
      if (currentMode.projection === 'flat') left.position.set(0, screenLayout.y, screenLayout.z);
      else if (currentMode.projection !== '360') left.rotation.y = Math.PI / 2;
      left.layers.set(stereo ? 1 : 0);
      seatRoot.add(left);
      videoMeshes.push(left);
      if (stereo) {
        const right = new THREE.Mesh(makeGeometry(currentMode.projection, 1), makeMaterial(1));
        right.name = 'jvr-right-video';
        if (currentMode.projection === 'flat') right.position.set(0, screenLayout.y, screenLayout.z);
        else if (currentMode.projection !== '360') right.rotation.y = Math.PI / 2;
        right.layers.set(2);
        seatRoot.add(right);
        videoMeshes.push(right);
      }
    }
    updateToolbar();
    drawPanel();
  }

  function replaceVideoTexture(video) {
    if (!video || !world) return;
    videoTexture?.dispose?.();
    videoTexture = new THREE.VideoTexture(video);
    videoTexture.minFilter = THREE.LinearFilter;
    videoTexture.magFilter = THREE.LinearFilter;
    videoTexture.generateMipmaps = false;
    videoTexture.colorSpace = THREE.SRGBColorSpace;
    rebuildVideoMeshes();
  }

  function configureEyeLayers() {
    if (!renderer?.xr?.isPresenting || !camera) return;
    const xrCamera = renderer.xr.getCamera(camera);
    const eyeCameras = xrCamera?.cameras || [];
    eyeCameras.forEach((eyeCamera, index) => {
      eyeCamera.layers.disableAll();
      eyeCamera.layers.enable(0);
      if (currentMode.stereo !== 'mono') eyeCamera.layers.enable(index === 0 ? 1 : 2);
    });
  }

  function setupControllers() {
    for (let index = 0; index < 2; index += 1) {
      const controller = renderer.xr.getController(index);
      const beam = new THREE.Mesh(
        new THREE.CylinderGeometry(0.0028, 0.0028, 5, 8, 1, true),
        new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 1, depthTest: false, depthWrite: false })
      );
      beam.name = 'jvr-ray';
      beam.rotation.x = -Math.PI / 2;
      beam.position.z = -2.5;
      beam.renderOrder = 9998;
      beam.visible = false;
      controller.add(beam);
      controller.addEventListener('selectstart', () => beginTrigger(controller));
      controller.addEventListener('selectend', () => endTrigger(controller));
      controller.addEventListener('squeezestart', () => {
        panelVisible ? hidePanel() : showPanel();
        const oldTimer = gripTimers.get(controller);
        if (oldTimer) clearTimeout(oldTimer);
        gripTimers.set(controller, setTimeout(() => {
          gripTimers.delete(controller);
          resetAll();
        }, 1200));
      });
      controller.addEventListener('squeezeend', () => {
        const timer = gripTimers.get(controller);
        if (timer) clearTimeout(timer);
        gripTimers.delete(controller);
      });
      world.add(controller);
      controllers.push(controller);
    }
  }

  function raycastPanel(controller) {
    if (!panelVisible || !panelMesh?.visible) return null;
    const rotation = new THREE.Matrix4().extractRotation(controller.matrixWorld);
    const raycaster = new THREE.Raycaster();
    raycaster.ray.origin.setFromMatrixPosition(controller.matrixWorld);
    raycaster.ray.direction.set(0, 0, -1).applyMatrix4(rotation);
    return raycaster.intersectObject(panelMesh, false)[0] || null;
  }

  function panelActionFromIntersection(intersection) {
    if (!intersection?.uv) return '';
    const x = intersection.uv.x * panelCanvas.width;
    const y = (1 - intersection.uv.y) * panelCanvas.height;
    return panelButtons.find((button) => x >= button.x && x <= button.x + button.width && y >= button.y && y <= button.y + button.height)?.action || '';
  }

  function isTimelineArea(intersection) {
    const point = panelPoint(intersection);
    return Boolean(point
      && point.x >= TIMELINE.x - 35
      && point.x <= TIMELINE.x + TIMELINE.width + 35
      && point.y >= TIMELINE.y - 28
      && point.y <= TIMELINE.y + TIMELINE.height);
  }

  function timelineTimeFromIntersection(intersection) {
    const point = panelPoint(intersection);
    if (!point || !getDuration()) return getCurrentTime();
    const ratio = THREE.MathUtils.clamp((point.x - TIMELINE.x) / TIMELINE.width, 0, 1);
    return ratio * getDuration();
  }

  function panelPoint(intersection) {
    if (!intersection?.uv) return null;
    return {
      x: intersection.uv.x * panelCanvas.width,
      y: (1 - intersection.uv.y) * panelCanvas.height
    };
  }

  function isPanelDragArea(intersection) {
    const point = panelPoint(intersection);
    if (!point) return false;
    const edge = 62;
    return point.x < edge || point.x > panelCanvas.width - edge
      || point.y < 105 || point.y > panelCanvas.height - edge;
  }

  function controllerDirection(controller) {
    const direction = new THREE.Vector3(0, 0, -1);
    const quaternion = new THREE.Quaternion();
    controller.getWorldQuaternion(quaternion);
    return direction.applyQuaternion(quaternion).normalize();
  }

  function beginTrigger(controller) {
    if (dragState.controller) return;
    const intersection = raycastPanel(controller);
    const action = panelActionFromIntersection(intersection);
    dragState.controller = controller;
    dragState.type = panelVisible && intersection && isTimelineArea(intersection)
      ? 'seek'
      : panelVisible && intersection && isPanelDragArea(intersection) && !action
        ? 'panel'
        : panelVisible ? 'panel-click' : 'view';
    dragState.startTime = performance.now();
    dragState.startDirection = controllerDirection(controller);
    dragState.startYaw = viewYaw;
    dragState.startPitch = viewPitch;
    dragState.startPanelPosition = panelMesh?.position.clone() || null;
    dragState.startPanelQuaternion = panelMesh?.quaternion.clone() || null;
    dragState.seekTime = dragState.type === 'seek' ? timelineTimeFromIntersection(intersection) : getCurrentTime();
    dragState.moved = false;
    triggerStates.set(controller, { intersection, action });
  }

  function updateTriggerDrag() {
    const controller = dragState.controller;
    if (!controller) return;
    const elapsed = performance.now() - dragState.startTime;
    if (dragState.type === 'seek') {
      const intersection = raycastPanel(controller);
      if (intersection) {
        const nextTime = timelineTimeFromIntersection(intersection);
        dragState.moved = dragState.moved || Math.abs(nextTime - dragState.seekTime) > Math.max(1, getDuration() * 0.002);
        dragState.seekTime = nextTime;
        drawPanel();
      }
      return;
    }
    if (dragState.type === 'panel') {
      const direction = controllerDirection(controller);
      const distance = dragState.startPanelPosition?.length() || 1.35;
      const targetWorld = new THREE.Vector3().copy(direction).multiplyScalar(distance);
      const cameraWorld = new THREE.Vector3();
      camera.getWorldPosition(cameraWorld);
      targetWorld.add(cameraWorld);
      camera.worldToLocal(targetWorld);
      if (panelMesh && dragState.startPanelPosition) {
        panelMesh.position.lerp(targetWorld, 0.35);
        panelMesh.lookAt(cameraWorld);
        dragState.moved = dragState.moved || panelMesh.position.distanceToSquared(dragState.startPanelPosition) > 0.0004;
      }
      return;
    }
    if (dragState.type !== 'view' || elapsed < 280 || !videoRoot) return;
    const current = controllerDirection(controller);
    const start = dragState.startDirection;
    const startYaw = Math.atan2(start.x, -start.z);
    const currentYaw = Math.atan2(current.x, -current.z);
    const startPitch = Math.asin(THREE.MathUtils.clamp(start.y, -1, 1));
    const currentPitch = Math.asin(THREE.MathUtils.clamp(current.y, -1, 1));
    viewYaw = dragState.startYaw - (currentYaw - startYaw) * 1.35;
    viewPitch = environmentActive()
      ? 0
      : THREE.MathUtils.clamp(dragState.startPitch + (currentPitch - startPitch) * 1.1, -Math.PI / 2, Math.PI / 2);
    applyViewRotation();
    dragState.moved = dragState.moved || Math.abs(currentYaw - startYaw) > 0.025 || Math.abs(currentPitch - startPitch) > 0.025;
  }

  function endTrigger(controller) {
    if (dragState.controller !== controller) return;
    const elapsed = performance.now() - dragState.startTime;
    const stored = triggerStates.get(controller) || {};
    if (dragState.type === 'seek') {
      seekAbsolute(dragState.seekTime);
      drawPanel();
    } else if (dragState.type === 'panel-click' && !dragState.moved) {
      const currentIntersection = raycastPanel(controller);
      const currentAction = panelActionFromIntersection(currentIntersection);
      if (stored.action && currentAction === stored.action) runAction(currentAction);
      else if (!stored.action && elapsed < 650) hidePanel();
    } else if (dragState.type === 'panel' && !dragState.moved && elapsed < 650) {
      hidePanel();
    } else if (dragState.type === 'view' && elapsed < 280 && !dragState.moved) {
      showPanel();
    }
    triggerStates.delete(controller);
    dragState.controller = null;
    dragState.type = '';
    dragState.startDirection = null;
    dragState.startPanelPosition = null;
    dragState.startPanelQuaternion = null;
    dragState.seekTime = 0;
    dragState.moved = false;
  }

  function updateControllerHover() {
    if (!panelVisible) {
      if (panelCursor) panelCursor.visible = false;
      return;
    }
    let nextHover = '';
    let cursorIntersection = null;
    for (const controller of controllers) {
      const intersection = raycastPanel(controller);
      const ray = controller.getObjectByName('jvr-ray');
      if (ray) {
        const distance = intersection ? Math.max(0.03, intersection.distance) : 5;
        ray.scale.y = distance / 5;
        ray.position.z = -distance / 2;
      }
      if (intersection && !cursorIntersection) cursorIntersection = intersection;
      nextHover = panelActionFromIntersection(intersection);
      if (nextHover) { cursorIntersection = intersection; break; }
    }
    if (panelCursor) {
      panelCursor.visible = Boolean(cursorIntersection?.uv);
      if (cursorIntersection?.uv) {
        panelCursor.position.x = (cursorIntersection.uv.x - 0.5) * PANEL_WIDTH;
        panelCursor.position.y = (cursorIntersection.uv.y - 0.5) * PANEL_HEIGHT;
      }
    }
    if (nextHover !== hoverAction) {
      hoverAction = nextHover;
      drawPanel();
    }
  }

  function pollGamepads() {
    if (!xrSession) return;
    for (const inputSource of xrSession.inputSources) {
      const gamepad = inputSource.gamepad;
      if (!gamepad) continue;
      const previous = inputStates.get(inputSource) || [];
      const pressed = gamepad.buttons.map((button) => button.pressed);
      if (pressed[4] && !previous[4]) runAction('play');
      if (pressed[5] && !previous[5]) panelVisible ? hidePanel() : showPanel();
      // Index 3 is the thumbstick click in the xr-standard mapping.
      if (pressed[3] && !previous[3]) recenterView();
      const x = Math.abs(gamepad.axes[2] || 0) > Math.abs(gamepad.axes[0] || 0) ? gamepad.axes[2] : gamepad.axes[0];
      if (Date.now() - lastStickAction > 650) {
        if (x > 0.8) { lastStickAction = Date.now(); runAction('forward'); }
        else if (x < -0.8) { lastStickAction = Date.now(); runAction('back'); }
      }
      inputStates.set(inputSource, pressed);
    }
  }

  function showPanel() {
    panelVisible = true;
    if (panelMesh) panelMesh.visible = true;
    controllers.forEach((controller) => {
      const ray = controller.getObjectByName('jvr-ray');
      if (ray) ray.visible = true;
    });
    drawPanel();
  }

  function hidePanel() {
    panelVisible = false;
    hoverAction = '';
    if (panelMesh) panelMesh.visible = false;
    if (panelCursor) panelCursor.visible = false;
    controllers.forEach((controller) => {
      const ray = controller.getObjectByName('jvr-ray');
      if (ray) { ray.visible = false; ray.scale.y = 1; ray.position.z = -2.5; }
    });
  }

  // Recentring in a headset means bringing the content to the viewer, never
  // moving the viewer: the XR camera pose is owned by the runtime and writing
  // to camera.position has no effect while presenting. So read where the head
  // actually is in the tracking space and put videoRoot's origin there.
  function headPose() {
    if (!renderer?.xr?.isPresenting || !camera) return null;
    const xrCamera = renderer.xr.getCamera(camera);
    if (!xrCamera) return null;
    const position = new THREE.Vector3();
    const quaternion = new THREE.Quaternion();
    xrCamera.getWorldPosition(position);
    xrCamera.getWorldQuaternion(quaternion);
    const forward = new THREE.Vector3(0, 0, -1).applyQuaternion(quaternion);
    return {
      position,
      quaternion,
      yaw: Math.atan2(-forward.x, -forward.z),
      pitch: Math.asin(THREE.MathUtils.clamp(forward.y, -1, 1))
    };
  }

  function applyViewRotation() {
    if (!videoRoot) return;
    const aim = new THREE.Quaternion().setFromEuler(new THREE.Euler(viewPitch, viewYaw, 0));
    if (viewBase) videoRoot.quaternion.copy(viewBase).multiply(aim);
    else videoRoot.quaternion.copy(aim);
  }

  // Two kinds of recentre, picked by where the head is looking. Upright, only
  // yaw is taken: a pitched or rolled head would tip the room off its own
  // floor. Looking well up or down, the viewer is lying back, so the whole room
  // tips onto the gaze — screen straight ahead of the eyes, seat under the
  // head — and the floor follows the head rather than the real floor.
  function recenterView() {
    if (!videoRoot) return;
    const pose = headPose();
    const reclined = Boolean(pose) && Math.abs(pose.pitch) > RECLINE_PITCH;
    viewBase = reclined ? pose.quaternion : null;
    viewYaw = pose && !reclined ? pose.yaw : 0;
    viewPitch = 0;
    applyViewRotation();
    if (reclined) {
      videoRoot.position.copy(pose.position);
    } else {
      // y stays pinned to eye height: environmentRoot hangs below videoRoot at
      // -EYE_HEIGHT, which is what keeps the auditorium floor on the real floor.
      videoRoot.position.set(pose ? pose.position.x : 0, EYE_HEIGHT, pose ? pose.position.z : 0);
    }
    if (statusEl) {
      statusEl.textContent = !pose
        ? 'View orientation reset.'
        : reclined
          ? 'Reclined view: the screen now sits where you are looking.'
          : 'View recentred on your current head pose.';
    }
    drawPanel();
  }

  function resetAll() {
    recenterView();
    if (panelMesh) {
      panelMesh.position.set(PANEL_HOME.x, PANEL_HOME.y, PANEL_HOME.z);
      panelMesh.quaternion.identity();
      panelMesh.scale.set(1, 1, 1);
    }
    showPanel();
    if (statusEl) statusEl.textContent = 'View recentred and control panel reset.';
    drawPanel();
  }

  function renderFrame(time) {
    if (!renderer || !world || !camera) return;
    configureEyeLayers();
    pollGamepads();
    updateTriggerDrag();
    updateControllerHover();
    if (time - lastPanelDraw > 250) {
      lastPanelDraw = time;
      drawPanel();
    }
    renderer.render(world, camera);
  }

  function bindMediaEvents(video, enabled) {
    if (!video) return;
    const method = enabled ? 'addEventListener' : 'removeEventListener';
    video[method]('timeupdate', handleMediaTimeUpdate);
    video[method]('play', handleMediaPlay);
    video[method]('pause', handleMediaPause);
    video[method]('loadedmetadata', handleMediaReady);
    video[method]('canplay', handleMediaReady);
    video[method]('error', handleMediaError);
  }

  function handleMediaTimeUpdate() {
    if (this !== activeVideo) return;
    drawPanel();
  }

  function handleMediaPlay() {
    if (this !== activeVideo) return;
    handleMediaReady.call(this);
    reportProgress('unpause');
  }

  function handleMediaPause() {
    if (this !== activeVideo) return;
    drawPanel();
    reportProgress('pause');
  }

  function handleMediaReady() {
    if (this !== activeVideo) return;
    replaceVideoTexture(activeVideo);
    autoDetectMode();
    const label = sourceMode === 'compat' ? 'H.264/8-bit compatibility source' : 'Original source';
    statusEl.textContent = `${label} connected · ${activeVideo.videoWidth || '?'}×${activeVideo.videoHeight || '?'} · ready to enter VR`;
    drawPanel();
  }

  function handleMediaError() {
    if (this !== activeVideo) return;
    const code = activeVideo.error?.code;
    statusEl.textContent = `Video source playback failed${code ? ` (MediaError ${code})` : ''}. Check the Jellyfin transcoding log.`;
  }

  function currentQuality() {
    return QUALITY_PRESETS.find((preset) => preset.id === qualityId) || QUALITY_PRESETS[0];
  }

  function readStoredQuality() {
    try {
      const value = localStorage.getItem(QUALITY_STORE_KEY);
      return QUALITY_PRESETS.some((preset) => preset.id === value) ? value : 'max';
    } catch (_) {
      return 'max';
    }
  }

  function rememberQuality() {
    try { localStorage.setItem(QUALITY_STORE_KEY, qualityId); } catch (_) { }
  }

  // The cap is square rather than 16:9 on purpose. Jellyfin scales to fit the
  // box while preserving aspect, and VR frames are routinely square (180 mono)
  // or taller than wide (over-under), both of which a 3840x2160 box would
  // shrink far harder than the tier name suggests.
  function applyQualityParams(url) {
    const quality = currentQuality();
    setQueryParam(url, 'MaxWidth', String(quality.cap));
    setQueryParam(url, 'MaxHeight', String(quality.cap));
    setQueryParam(url, 'VideoBitRate', String(quality.bitrate));
  }

  // Quality only means something for our own transcode: in "original" mode the
  // stream was negotiated by Jellyfin's player and is not ours to re-request,
  // so choosing a quality also switches over to the compatibility source.
  function applyQuality(id, remember = true) {
    if (!QUALITY_PRESETS.some((preset) => preset.id === id)) return;
    qualityId = id;
    if (remember) rememberQuality();
    updateToolbar();
    drawPanel();
    if (!compatUrl) {
      if (statusEl) statusEl.textContent = 'Quality needs the H.264 compatibility source, which this stream URL cannot provide.';
      return;
    }
    startCompatAt(getCurrentTime(), !activeVideo?.paused).catch((error) => {
      if (statusEl) statusEl.textContent = `Quality change failed: ${error?.message || error}`;
    });
  }

  function cycleQuality() {
    const index = QUALITY_PRESETS.findIndex((preset) => preset.id === qualityId);
    applyQuality(QUALITY_PRESETS[(index + 1) % QUALITY_PRESETS.length].id);
  }

  async function startCompatAt(position, shouldPlay = true) {
    if (!compatUrl) throw new Error('Cannot derive a Jellyfin H.264 compatibility source from this URL');
    const serial = ++loadSerial;
    const duration = getDuration();
    const target = Math.max(0, Math.min(Number(position) || 0, duration ? duration - 0.25 : Infinity));
    const url = new URL(compatUrl);
    setQueryParam(url, 'StartTimeTicks', Math.round(target * 10000000));
    applyQualityParams(url);
    const previousSessionId = compatSessionId;
    compatSessionId = newCompatSessionId();
    setQueryParam(url, 'PlaySessionId', compatSessionId);
    const muted = activeVideo?.muted ?? sourceVideo.muted;
    const volume = activeVideo?.volume ?? sourceVideo.volume;
    bindMediaEvents(activeVideo, false);
    activeVideo?.pause?.();
    if (compatVideo) {
      compatVideo.removeAttribute('src');
      compatVideo.load();
      compatVideo.remove();
    }
    stopEncoding(previousSessionId);
    compatVideo = document.createElement('video');
    compatVideo.id = 'jvr-compat-video';
    compatVideo.preload = 'auto';
    compatVideo.playsInline = true;
    compatVideo.setAttribute('playsinline', '');
    compatVideo.setAttribute('webkit-playsinline', '');
    compatVideo.style.cssText = 'position:fixed;left:0;bottom:0;width:2px;height:2px;opacity:.01;pointer-events:none;z-index:-1';
    compatVideo.muted = muted;
    compatVideo.volume = volume;
    compatOffset = target;
    sourceMode = 'compat';
    activeVideo = compatVideo;
    bindMediaEvents(activeVideo, true);
    overlay.appendChild(compatVideo);
    replaceVideoTexture(activeVideo);
    updateToolbar();
    statusEl.textContent = `Starting H.264/8-bit transcode (${currentQuality().label}) from ${formatTime(target)}...`;
    compatVideo.src = url.href;
    compatVideo.load();
    lastReportKey = '';
    startProgressReporting();
    if (shouldPlay) {
      try {
        await compatVideo.play();
      } catch (_) {
        if (serial === loadSerial) statusEl.textContent = 'Compatibility source ready. Press PLAY to start.';
      }
    }
    reportProgress('timeupdate');
  }

  async function useOriginalSource() {
    const position = getCurrentTime();
    const shouldPlay = !activeVideo?.paused;
    reportProgress('timeupdate');
    stopProgressReporting();
    stopCompatEncoding();
    const muted = activeVideo?.muted ?? false;
    const volume = activeVideo?.volume ?? 1;
    ++loadSerial;
    bindMediaEvents(activeVideo, false);
    activeVideo?.pause?.();
    sourceMode = 'original';
    sourceVideo.currentTime = Math.max(0, Math.min(position, sourceVideo.duration || position));
    sourceVideo.muted = muted;
    sourceVideo.volume = volume;
    activeVideo = sourceVideo;
    bindMediaEvents(activeVideo, true);
    replaceVideoTexture(activeVideo);
    updateToolbar();
    statusEl.textContent = 'Switched back to the original source.';
    if (shouldPlay) await sourceVideo.play().catch(() => { });
  }

  function toggleSource() {
    if (!compatUrl) return;
    const position = getCurrentTime();
    const shouldPlay = !activeVideo?.paused;
    if (sourceMode === 'compat') useOriginalSource();
    else startCompatAt(position, shouldPlay).catch((error) => {
      statusEl.textContent = `Compatibility source failed to start: ${error?.message || error}`;
    });
  }

  function seekAbsolute(position) {
    const target = Math.max(0, Math.min(Number(position) || 0, getDuration() || Infinity));
    if (sourceMode !== 'compat') {
      activeVideo.currentTime = target;
      return;
    }
    const relative = target - compatOffset;
    if (relative >= 0 && Number.isFinite(activeVideo.duration) && relative <= activeVideo.duration) {
      activeVideo.currentTime = relative;
      reportProgress('timeupdate');
    } else {
      startCompatAt(target, !activeVideo.paused).catch((error) => {
        statusEl.textContent = `Compatibility source seek failed: ${error?.message || error}`;
      });
    }
  }

  function cycleProjection() {
    const values = ['360', '180', 'fisheye', 'flat'];
    applyMode({ projection: values[(values.indexOf(currentMode.projection) + 1) % values.length] }, true);
  }

  function cycleStereo() {
    const values = ['mono', 'sbs', 'ou'];
    applyMode({ stereo: values[(values.indexOf(currentMode.stereo) + 1) % values.length] }, true);
  }

  function runAction(action) {
    if (!activeVideo) return;
    if (action === 'play') activeVideo.paused ? activeVideo.play().catch(() => { }) : activeVideo.pause();
    else if (action === 'back') seekAbsolute(getCurrentTime() - 10);
    else if (action === 'forward') seekAbsolute(getCurrentTime() + 10);
    else if (action === 'mute') activeVideo.muted = !activeVideo.muted;
    else if (action === 'projection') cycleProjection();
    else if (action === 'stereo') cycleStereo();
    else if (action === 'environment') cycleEnvironment();
    else if (action === 'lights') { if (environmentActive()) cycleLights(); }
    else if (action === 'seat') { if (environmentActive()) cycleSeat(); }
    else if (action === 'swap') applyMode({ swap: !currentMode.swap }, true);
    else if (action === 'source') toggleSource();
    else if (action === 'quality') cycleQuality();
    else if (action === 'recenter') recenterView();
    else if (action === 'reset') resetAll();
    else if (action === 'hide') hidePanel();
    else if (action === 'exit-vr') exitVrOnly(true);
    drawPanel();
  }

  function updateToolbar() {
    if (!overlay) return;
    overlay.querySelectorAll('[data-action="projection"][data-value]').forEach((button) => {
      button.classList.toggle('active', button.dataset.value === currentMode.projection);
    });
    overlay.querySelectorAll('[data-action="stereo"][data-value]').forEach((button) => {
      button.classList.toggle('active', button.dataset.value === currentMode.stereo);
    });
    overlay.querySelectorAll('[data-action="environment"][data-value]').forEach((button) => {
      button.classList.toggle('active', button.dataset.value === environmentName);
    });
    const sourceButton = overlay.querySelector('#jvr-source-toggle');
    if (sourceButton) {
      sourceButton.textContent = sourceMode === 'compat' ? 'H.264 Compat ✓' : 'Original Source';
      sourceButton.classList.toggle('active', sourceMode === 'compat');
      sourceButton.disabled = !compatUrl;
    }
    const lightsSelect = overlay.querySelector('#jvr-lights-select');
    if (lightsSelect) {
      lightsSelect.value = lightLevelId;
      lightsSelect.disabled = !environmentActive();
    }
    const seatSelect = overlay.querySelector('#jvr-seat-select');
    if (seatSelect) {
      seatSelect.value = seatId;
      seatSelect.disabled = !environmentActive();
    }
    const qualitySelect = overlay.querySelector('#jvr-quality-select');
    if (qualitySelect) {
      qualitySelect.value = qualityId;
      qualitySelect.title = currentQuality().note;
      qualitySelect.disabled = !compatUrl;
    }
  }

  async function enterVr() {
    if (!window.isSecureContext) return alert('WebXR requires HTTPS.');
    if (!navigator.xr) return alert('This browser does not support WebXR.');
    if (!renderer) return alert('The WebXR renderer is not initialised yet.');
    try {
      // requestSession must run directly in the Enter VR click's user-activation task.
      const sessionPromise = navigator.xr.requestSession('immersive-vr', {
        optionalFeatures: ['local-floor', 'bounded-floor', 'hand-tracking']
      });
      activeVideo?.play?.().catch(() => { });
      xrSession = await sessionPromise;
      xrSession.addEventListener('end', () => {
        xrSession = null;
        hidePanel();
        if (!closing && statusEl) statusEl.textContent = 'Exited VR. You can re-enter or go back to Jellyfin.';
      }, { once: true });
      await renderer.xr.setSession(xrSession);
      statusEl.textContent = 'Native WebXR active. Press the trigger to open the control panel.';
      setTimeout(showPanel, 350);
    } catch (error) {
      alert(`Failed to enter VR: ${error?.message || error}`);
    }
  }

  async function exitVrOnly(closeAfter = false) {
    try {
      const session = xrSession || renderer?.xr?.getSession?.();
      if (session) await session.end();
    } catch (_) { }
    if (closeAfter) closePlayer();
  }

  function handleToolbar(event) {
    const button = event.target.closest('button[data-action]');
    if (!button) return;
    const { action, value } = button.dataset;
    if (action === 'exit') closePlayer();
    else if (action === 'enter') enterVr();
    else if (action === 'projection') applyMode({ projection: value }, true);
    else if (action === 'stereo') applyMode({ stereo: value }, true);
    else if (action === 'environment') applyEnvironment(value, true);
    else if (action === 'swap') applyMode({ swap: !currentMode.swap }, true);
    else if (action === 'source') toggleSource();
  }

  function handleToolbarChange(event) {
    const select = event.target.closest('select[data-action]');
    if (!select) return;
    if (select.dataset.action === 'quality') applyQuality(select.value);
    else if (select.dataset.action === 'lights') applyLights(select.value);
    else if (select.dataset.action === 'seat') applySeatChoice(select.value);
  }

  async function openPlayer() {
    if (overlay) return;
    sourceVideo = findVideo();
    if (!sourceVideo) return alert('Start playing a video in Jellyfin first.');
    try {
      activeVideo = sourceVideo;
      jellyfin = readJellyfinContext(sourceVideo);
      compatUrl = buildCompatStreamUrl(sourceVideo);
      detectionDone = false;
      itemTextReady = false;
      itemText = '';
      detectionMessage = '';
      environmentName = readStoredEnvironment();
      lightLevelId = readStoredChoice(LIGHT_STORE_KEY, LIGHT_LEVELS, 'low');
      seatId = readStoredChoice(SEAT_STORE_KEY, SEATS, 'mid');
      qualityId = readStoredQuality();
      detectionTimer = setTimeout(() => {
        itemTextReady = true;
        autoDetectMode();
      }, METADATA_TIMEOUT);
      loadItemText();
      originalState = {
        currentTime: sourceVideo.currentTime || 0,
        duration: sourceVideo.duration || 0,
        paused: sourceVideo.paused,
        muted: sourceVideo.muted,
        volume: sourceVideo.volume
      };
      await ensureThree();
      buildOverlay();
      savedVideoId = sourceVideo.id;
      if (!sourceVideo.id) sourceVideo.id = 'jvr-source-video';
      buildRenderer();
      bindMediaEvents(sourceVideo, true);
      replaceVideoTexture(sourceVideo);
      updateToolbar();
      statusEl.textContent = `Native WebXR ready · ${sourceVideo.videoWidth || '?'}×${sourceVideo.videoHeight || '?'}${detectionMessage ? ` · ${detectionMessage}` : ''}`;
      sourceVideo.play().catch(() => { });
    } catch (error) {
      closePlayer();
      alert(`VR player failed to initialise: ${error?.message || error}`);
    }
  }

  function closePlayer() {
    if (closing) return;
    closing = true;
    const finalTime = getCurrentTime();
    const finalMuted = activeVideo?.muted;
    const finalVolume = activeVideo?.volume;
    reportProgress('timeupdate');
    stopProgressReporting();
    stopCompatEncoding();
    const session = xrSession || renderer?.xr?.getSession?.();
    session?.end?.().catch?.(() => { });
    xrSession = null;
    ++loadSerial;
    bindMediaEvents(activeVideo, false);
    if (compatVideo) {
      compatVideo.pause();
      compatVideo.removeAttribute('src');
      compatVideo.load();
      compatVideo.remove();
      compatVideo = null;
    }
    if (sourceVideo) {
      bindMediaEvents(sourceVideo, false);
      if (Number.isFinite(finalTime)) sourceVideo.currentTime = Math.min(finalTime, sourceVideo.duration || finalTime);
      if (typeof finalMuted === 'boolean') sourceVideo.muted = finalMuted;
      if (Number.isFinite(finalVolume)) sourceVideo.volume = finalVolume;
      sourceVideo.id = savedVideoId;
      sourceVideo.play().catch(() => { });
    }
    renderer?.setAnimationLoop?.(null);
    disposeVideoMeshes();
    if (environmentRoot) {
      environmentRoot.parent?.remove(environmentRoot);
      disposeTree(environmentRoot);
    }
    videoTexture?.dispose?.();
    panelMesh?.geometry?.dispose?.();
    panelMesh?.material?.dispose?.();
    panelBorder?.geometry?.dispose?.();
    panelBorder?.material?.dispose?.();
    panelCursor?.traverse?.((object) => {
      if (object !== panelCursor) object.geometry?.dispose?.();
      if (object !== panelCursor) object.material?.dispose?.();
    });
    panelCursor?.geometry?.dispose?.();
    panelCursor?.material?.dispose?.();
    panelTexture?.dispose?.();
    controllers.forEach((controller) => {
      const gripTimer = gripTimers.get(controller);
      if (gripTimer) clearTimeout(gripTimer);
      gripTimers.delete(controller);
      controller.children.forEach((child) => {
        child.geometry?.dispose?.();
        child.material?.dispose?.();
      });
    });
    renderer?.dispose?.();
    if (resizeHandler) window.removeEventListener('resize', resizeHandler);
    overlay?.remove();
    overlay = null;
    statusEl = null;
    sourceVideo = null;
    activeVideo = null;
    originalState = null;
    compatUrl = '';
    compatOffset = 0;
    sourceMode = 'original';
    savedVideoId = '';
    jellyfin = null;
    lastReportKey = '';
    if (detectionTimer) clearTimeout(detectionTimer);
    detectionTimer = 0;
    detectionDone = false;
    itemTextReady = false;
    itemText = '';
    detectionMessage = '';
    world = null;
    camera = null;
    renderer = null;
    videoTexture = null;
    videoMeshes = [];
    videoRoot = null;
    seatRoot = null;
    environmentRoot = null;
    environmentBuilt = '';
    screenSurround = null;
    environmentFog = null;
    controllers = [];
    panelMesh = null;
    panelCanvas = null;
    panelContext = null;
    panelTexture = null;
    panelBorder = null;
    panelCursor = null;
    panelButtons = [];
    panelVisible = false;
    hoverAction = '';
    resizeHandler = null;
    dragState.controller = null;
    dragState.type = '';
    dragState.startDirection = null;
    dragState.startPanelPosition = null;
    dragState.startPanelQuaternion = null;
    dragState.seekTime = 0;
    dragState.moved = false;
    closing = false;
  }

  // The VR entry lives in the player's own options menu rather than in the
  // control bar, so nothing is added to the Jellyfin chrome until the user
  // opens that menu.
  //
  // Jellyfin builds action sheets from its own markup, and the class names
  // differ between versions and skins. Cloning a real item from the sheet we
  // are augmenting is what keeps the entry looking native without hard-coding
  // any of them — and, unlike the old control-bar anchor, it needs no
  // locale-specific selectors.
  function buildMenuItem(template) {
    const item = template.cloneNode(true);
    item.id = MENU_ITEM_ID;
    for (const attribute of ['data-id', 'data-command', 'data-itemid', 'data-index']) {
      item.removeAttribute(attribute);
    }
    item.setAttribute('title', MENU_ITEM_LABEL);
    const icon = item.querySelector('.material-icons');
    if (icon) {
      // Keep the structural classes and drop the one naming the template's
      // glyph; the icon font renders from the text content.
      icon.className = Array.from(icon.classList)
        .filter((name) => name === 'material-icons' || /listitem|actionsheet/i.test(name))
        .join(' ');
      icon.textContent = 'view_in_ar';
    }
    const label = item.querySelector('.actionSheetItemText,.listItemBodyText');
    if (label) {
      label.textContent = MENU_ITEM_LABEL;
    } else {
      item.childNodes.forEach((node) => { if (node.nodeType === 3) node.textContent = ''; });
      const span = document.createElement('span');
      span.textContent = MENU_ITEM_LABEL;
      item.append(span);
    }
    item.querySelectorAll('.listItemAside,.actionSheetItemAsideText,.secondary').forEach((node) => node.remove());
    item.addEventListener('click', () => {
      // Jellyfin's delegated handler closes the sheet on a click anywhere in a
      // menu item and resolves with our (absent) data-id, which its callers
      // ignore. Let that run and open once the dialog has torn down.
      setTimeout(openPlayer, 50);
    });
    return item;
  }

  // Only offer VR while a video is actually on screen. Jellyfin leaves a
  // <video> in the DOM after playback, so the element alone is not enough —
  // a collapsed one means we are back in the library, where an item's context
  // menu is also an action sheet and must not gain a VR entry.
  function playerActive() {
    const video = findVideo();
    if (!video) return false;
    const rect = video.getBoundingClientRect();
    return rect.width > 100 && rect.height > 100;
  }

  function augmentSheet(sheet) {
    if (sheet.dataset.jvrMenu === '1' || sheet.querySelector(`#${MENU_ITEM_ID}`)) return;
    const items = sheet.querySelectorAll(SHEET_ITEM_SELECTOR);
    const template = items[items.length - 1];
    if (!template?.parentElement) return;
    sheet.dataset.jvrMenu = '1';
    template.parentElement.append(buildMenuItem(template));
  }

  function scan() {
    if (!playerActive()) return;
    document.querySelectorAll(SHEET_SELECTOR).forEach(augmentSheet);
  }

  // The observer watches a very busy subtree, so coalesce bursts into one pass
  // per frame instead of running a query for every mutation.
  function scheduleScan() {
    if (scanScheduled) return;
    scanScheduled = true;
    requestAnimationFrame(() => {
      scanScheduled = false;
      scan();
    });
  }

  function init() {
    scan();
    new MutationObserver(scheduleScan).observe(document.documentElement, { childList: true, subtree: true });
    scanTimer = setInterval(scan, 1000);
    // Escape hatch: the menu entry depends on Jellyfin's action sheet markup,
    // so keep a way in that does not.
    window.jellyfinVR = { open: openPlayer, close: closePlayer };
    window.addEventListener('beforeunload', () => clearInterval(scanTimer), { once: true });
    window.addEventListener('pagehide', () => {
      reportProgress('timeupdate');
      stopProgressReporting();
      stopCompatEncoding();
    });
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init, { once: true });
  else init();
})();
