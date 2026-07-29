import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "@phosphor-icons/web/regular";
import "@phosphor-icons/web/fill";

// A range track cannot read its own value in CSS, so mirror the filled
// proportion onto the element and let the stylesheet paint up to it. Deriving
// it from the input's own min and max means every slider fills correctly
// without per-slider arithmetic at each call site.
function syncRangeFill(input: HTMLInputElement) {
  const min = Number(input.min || 0);
  const max = Number(input.max || 100);
  const span = max - min;
  const filled = span > 0 ? (Number(input.value) - min) / span : 0;
  input.style.setProperty("--range-fill", `${Math.min(Math.max(filled, 0), 1) * 100}%`);
}

function syncRangeFills(root: ParentNode = document) {
  root.querySelectorAll<HTMLInputElement>('input[type="range"]').forEach(syncRangeFill);
}

document.addEventListener(
  "input",
  (event) => {
    const target = event.target;
    if (target instanceof HTMLInputElement && target.type === "range") syncRangeFill(target);
  },
  true,
);

const tahoeCursorUrl = new URL("../src-tauri/assets/pikscreen-tahoe-pointer.svg", import.meta.url).href;
const routeParameters = new URLSearchParams(window.location.search);
const requestedSurface = routeParameters.get("surface");
const requestedEditorSession = routeParameters.get("editor");
const tauriAvailable = "__TAURI_INTERNALS__" in window;
const currentWindowLabel = tauriAvailable ? getCurrentWindow().label : "browser";
const settingsWindow = currentWindowLabel === "settings" || requestedSurface === "settings";
const editorWindow = currentWindowLabel === "editor" || (!tauriAvailable && requestedEditorSession !== null);

const backgroundPresets = [
  ["pureBlack", "Pure Black", null, "#000000"],
  ["pureWhite", "Pure White", null, "#ffffff"],
  ["tahoeDark", "Tahoe Dark", "tahoe-dark.jpg", null],
  ["tahoeLight", "Tahoe Light", "tahoe-light.jpg", null],
  ["midnight8", "Midnight", "midnight-8.jpg", null],
  ["sequoiaBlue", "Sequoia Blue", "sequoia-blue.jpg", null],
  ["sequoiaBlueOrange", "Sequoia Sunset", "sequoia-blue-orange.jpg", null],
  ["sonomaClouds", "Sonoma Clouds", "sonoma-clouds.jpg", null],
  ["sonomaDark", "Sonoma Dark", "sonoma-dark.jpg", null],
  ["sonomaLight", "Sonoma Light", "sonoma-light.jpg", null],
  ["sonomaEvening", "Sonoma Evening", "sonoma-evening.jpg", null],
  ["sonomaHorizon", "Sonoma Horizon", "sonoma-horizon.jpg", null],
  ["ventura", "Ventura", "ventura.jpg", null],
  ["venturaDark", "Ventura Dark", "ventura-dark.jpg", null],
  ["ipad17Dark", "iPad Dark", "ipad-17-dark.jpg", null],
  ["ipad17Light", "iPad Light", "ipad-17-light.jpg", null],
  ["glassmorphism3", "Glass 03", "glassmorphism-3.jpg", null],
  ["glassmorphism4", "Glass 04", "glassmorphism-4.jpg", null],
  ["energy19", "Energy 19", "energy-19.jpg", null],
  ["energy17", "Energy 17", "energy-17.jpg", null],
  ["wallpaper3", "Wallpaper 03", "wallpaper3.jpg", null],
  ["wallpaper4", "Wallpaper 04", "wallpaper4.jpg", null],
  ["wallpaper10", "Wallpaper 10", "wallpaper10.jpg", null],
  ["cityscape", "Cityscape", "cityscape.jpg", null],
  ["levels", "Levels", "levels.jpg", null],
  ["iridescent9", "Iridescent", "iridescent-9.jpg", null],
] as const;

function mountBackgroundGallery(
  container: HTMLElement | null,
  select: HTMLSelectElement,
  onSelect: (value: RecordingSettings["background"]) => void,
) {
  if (!container) return () => {};
  const buttons = backgroundPresets.map(([value, label, file, color]) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "wallpaper-tile";
    button.dataset.background = value;
    if (file) {
      // These are full resolution wallpapers. As CSS backgrounds all two dozen
      // decode at once when the panel opens and the interface stalls for a
      // couple of seconds; an img defers the ones that are off screen and keeps
      // the decode off the main thread.
      const thumbnail = document.createElement("img");
      thumbnail.className = "wallpaper-thumbnail";
      thumbnail.loading = "lazy";
      thumbnail.decoding = "async";
      thumbnail.alt = "";
      thumbnail.src = new URL(`../src-tauri/assets/wallpapers/${file}`, import.meta.url).href;
      button.append(thumbnail);
    } else {
      button.style.backgroundColor = color;
      button.classList.add("solid-color");
    }
    button.title = label;
    const text = document.createElement("span");
    text.textContent = label;
    button.append(text);
    button.addEventListener("click", () => {
      select.value = value;
      onSelect(value);
      sync();
    });
    return button;
  });
  container.replaceChildren(...buttons);
  const sync = () => {
    buttons.forEach((button) => {
      const selected = button.dataset.background === select.value;
      button.classList.toggle("selected", selected);
      button.setAttribute("aria-pressed", String(selected));
    });
  };
  sync();
  return sync;
}

type ProcessingProgress = { stage: string; percent: number };
type Crop = { width: number; height: number; excludedBottom: number };
type CaptureSourceKind = "monitor" | "window";
type PreviewInfo = { detail: string; crop: Crop; sourceKind: CaptureSourceKind };
type PreviewSettings = { hideBottomBar: boolean; barHeightCorrection: number };
type RecordingSettings = {
  fps: number;
  quality: "balanced" | "high" | "ultra";
  background: "pureBlack" | "pureWhite" | "tahoeLight" | "tahoeDark" | "midnight8" | "ipad17Dark" | "ipad17Light" | "sequoiaBlue" | "sequoiaBlueOrange" | "ventura" | "sonomaClouds" | "sonomaLight" | "sonomaDark" | "glassmorphism3" | "glassmorphism4" | "energy19" | "wallpaper3" | "wallpaper4" | "cityscape" | "levels" | "wallpaper10" | "venturaDark" | "sonomaEvening" | "sonomaHorizon" | "iridescent9" | "energy17";
  backgroundEnabled: boolean;
  customBackgroundPath: string | null;
  cursorVisible: boolean;
  customCursorPath: string | null;
  cursorSmoothing: number;
  cursorSize: number;
  zoomScale: number;
  clickEffectsEnabled: boolean;
  audio: { systemEnabled: boolean; microphoneEnabled: boolean; systemVolume: number; microphoneVolume: number; systemDevice: string | null; microphoneDevice: string | null };
  webcam: { enabled: boolean; deviceId: string | null; mirror: boolean; sizePercent: number; margin: number; roundness: number; shadow: number; reactToZoom: boolean };
};
type SavedSettings = RecordingSettings & PreviewSettings;
type AudioDevice = { id: string; label: string };
type AudioDevices = { systemOutputs: AudioDevice[]; microphones: AudioDevice[]; defaultSystemOutput: string | null; defaultMicrophone: string | null };
type WebcamDevice = { id: string; label: string };
type AppearanceAsset = { path: string; label: string };
type SessionPhase = "ready" | "preview" | "recording" | "rendering";
type ZoomMarker = { timeMs: number; x: number; y: number; targetScale: number | null; heldUntilMs: number | null; hideCursor: boolean };
type CursorSample = { timeMs: number; x: number; y: number; kind: string };
type ClickSample = { timeMs: number; x: number; y: number; primary: boolean };
type Segment = { startMs: number; endMs: number };
type EditorSession = {
  id: string;
  width: number;
  height: number;
  durationMs: number;
  trimStartMs: number;
  trimEndMs: number;
  segments: Segment[];
  audioSegments: Segment[];
  markers: ZoomMarker[];
  cursorSamples: CursorSample[];
  clickSamples: ClickSample[];
  syntheticCursor: boolean;
  settings: RecordingSettings;
  hasWebcam: boolean;
  videoUrl: string;
  audioUrl: string | null;
  webcamUrl: string | null;
  customBackgroundUrl: string | null;
  customCursorUrl: string | null;
};
type EditorChanges = {
  markers: ZoomMarker[];
  trimStartMs: number;
  trimEndMs: number;
  segments: Segment[];
  audioSegments: Segment[];
  settings: RecordingSettings;
};
type EditorSnapshot = Pick<
  EditorSession,
  "trimStartMs" | "trimEndMs" | "segments" | "audioSegments" | "markers" | "settings"
>;

function formatElapsed(milliseconds: number) {
  const tenths = Math.floor(milliseconds / 100) % 10;
  const totalSeconds = Math.floor(milliseconds / 1000);
  return `${String(Math.floor(totalSeconds / 60)).padStart(2, "0")}:${String(totalSeconds % 60).padStart(2, "0")}.${tenths}`;
}

type PreviewCamera = {
  scale: number;
  cropX: number;
  cropY: number;
  hideCursor: boolean;
};
type PreviewSpringState = { value: number; velocity: number; initialized: boolean };
type PreviewCameraFrame = { timeMs: number; camera: PreviewCamera };

type PreviewZoomEvent = {
  startMs: number;
  transitionEndMs: number;
  holdUntilMs: number;
  endMs: number;
  centerX: number;
  centerY: number;
  targetScale: number;
  hideCursor: boolean;
  fromScale: number;
  fromCropX: number;
  fromCropY: number;
};

const previewZoomTiming = { inMs: 190, holdMs: 2_620, outMs: 190 };
const identityCamera = (): PreviewCamera => ({ scale: 1, cropX: 0, cropY: 0, hideCursor: false });
const normalizedProgress = (value: number, start: number, end: number) =>
  end <= start ? 1 : Math.max(0, Math.min(1, (value - start) / (end - start)));
const recordlyEaseOutZoom = (value: number) => {
  const progress = Math.max(0, Math.min(1, value));
  let t = progress;
  for (let iteration = 0; iteration < 8; iteration += 1) {
    const x = 0.48 * t - 0.06 * t * t + 0.58 * t * t * t;
    const derivative = 0.48 - 0.12 * t + 1.74 * t * t;
    if (Math.abs(derivative) < 0.000001) break;
    t = Math.max(0, Math.min(1, t - (x - progress) / derivative));
  }
  let lower = 0;
  let upper = 1;
  for (let iteration = 0; iteration < 10; iteration += 1) {
    const x = 0.48 * t - 0.06 * t * t + 0.58 * t * t * t;
    if (Math.abs(x - progress) < 0.000001) break;
    if (x < progress) lower = t;
    else upper = t;
    t = (lower + upper) / 2;
  }
  return 1 - (1 - t) ** 3;
};
const targetCamera = (x: number, y: number, scale: number): PreviewCamera => ({
  scale,
  cropX: Math.max(0, Math.min(1 - 1 / scale, x - 0.5 / scale)),
  cropY: Math.max(0, Math.min(1 - 1 / scale, y - 0.5 / scale)),
  hideCursor: false,
});
const cameraForEvent = (event: PreviewZoomEvent, timeMs: number): PreviewCamera => {
  if (timeMs < event.startMs || timeMs >= event.endMs) return identityCamera();
  const target = targetCamera(event.centerX, event.centerY, event.targetScale);
  if (timeMs <= event.transitionEndMs) {
    const progress = recordlyEaseOutZoom(normalizedProgress(timeMs, event.startMs, event.transitionEndMs));
    if (event.fromScale === 1 && event.fromCropX === 0 && event.fromCropY === 0) {
      const scale = 1 + (event.targetScale - 1) * progress;
      const crop = (focus: number) =>
        Math.max(0, Math.min(1 - 1 / scale, (focus * event.targetScale - 0.5) * progress / scale));
      return { scale, cropX: crop(event.centerX), cropY: crop(event.centerY), hideCursor: event.hideCursor };
    }
    return {
      scale: event.fromScale + (target.scale - event.fromScale) * progress,
      cropX: event.fromCropX + (target.cropX - event.fromCropX) * progress,
      cropY: event.fromCropY + (target.cropY - event.fromCropY) * progress,
      hideCursor: event.hideCursor,
    };
  }
  if (timeMs <= event.holdUntilMs) {
    return { ...target, hideCursor: event.hideCursor };
  }
  const remaining = 1 - recordlyEaseOutZoom(normalizedProgress(timeMs, event.holdUntilMs, event.endMs));
  const scale = 1 + (event.targetScale - 1) * remaining;
  const crop = (focus: number) =>
    Math.max(0, Math.min(1 - 1 / scale, (focus * event.targetScale - 0.5) * remaining / scale));
  return { scale, cropX: crop(event.centerX), cropY: crop(event.centerY), hideCursor: event.hideCursor };
};
const buildPreviewZoomEvents = (markers: ZoomMarker[], defaultScale: number) => {
  const events: PreviewZoomEvent[] = [];
  [...markers].sort((left, right) => left.timeMs - right.timeMs).forEach((marker) => {
    const targetScale = marker.targetScale && marker.targetScale >= 1.2 && marker.targetScale <= 2.4
      ? marker.targetScale
      : defaultScale;
    const margin = 1 / (2 * Math.max(1, targetScale));
    const transitionEndMs = marker.timeMs + previewZoomTiming.inMs;
    const holdUntilMs = marker.heldUntilMs === null
      ? transitionEndMs + previewZoomTiming.holdMs
      : Math.max(marker.heldUntilMs, transitionEndMs);
    const previous = events[events.length - 1];
    const from = previous ? cameraForEvent(previous, marker.timeMs) : identityCamera();
    if (previous && marker.timeMs < previous.endMs) previous.endMs = marker.timeMs;
    events.push({
      startMs: marker.timeMs,
      transitionEndMs,
      holdUntilMs,
      endMs: holdUntilMs + previewZoomTiming.outMs,
      centerX: Math.max(margin, Math.min(1 - margin, marker.x)),
      centerY: Math.max(margin, Math.min(1 - margin, marker.y)),
      targetScale,
      hideCursor: marker.hideCursor,
      fromScale: from.scale,
      fromCropX: from.cropX,
      fromCropY: from.cropY,
    });
  });
  return events;
};
const previewCameraAt = (events: PreviewZoomEvent[], timeMs: number) => {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index];
    if (timeMs >= event.startMs && timeMs < event.endMs) return cameraForEvent(event, timeMs);
  }
  return identityCamera();
};
const createPreviewSpring = (value: number): PreviewSpringState => ({
  value,
  velocity: 0,
  initialized: false,
});
const resolvePreviewSpringPosition = (
  seconds: number,
  target: number,
  initialDelta: number,
  initialVelocity: number,
  dampingRatio: number,
  undampedFrequency: number,
) => {
  if (dampingRatio < 1) {
    const dampedFrequency = undampedFrequency * Math.sqrt(1 - dampingRatio ** 2);
    const envelope = Math.exp(-dampingRatio * undampedFrequency * seconds);
    return target - envelope * (
      ((initialVelocity + dampingRatio * undampedFrequency * initialDelta) / dampedFrequency)
        * Math.sin(dampedFrequency * seconds)
      + initialDelta * Math.cos(dampedFrequency * seconds)
    );
  }
  if (dampingRatio === 1) {
    return target - Math.exp(-undampedFrequency * seconds)
      * (initialDelta + (initialVelocity + undampedFrequency * initialDelta) * seconds);
  }
  const dampedFrequency = undampedFrequency * Math.sqrt(dampingRatio ** 2 - 1);
  const envelope = Math.exp(-dampingRatio * undampedFrequency * seconds);
  const frequencyTime = Math.min(dampedFrequency * seconds, 300);
  return target - envelope * (
    (initialVelocity + dampingRatio * undampedFrequency * initialDelta) * Math.sinh(frequencyTime)
    + dampedFrequency * initialDelta * Math.cosh(frequencyTime)
  ) / dampedFrequency;
};
const stepPreviewSpring = (state: PreviewSpringState, target: number, deltaMs: number) => {
  const safeDeltaMs = !Number.isFinite(deltaMs) || deltaMs <= 0
    ? 1_000 / 60
    : Math.max(1, Math.min(80, deltaMs));
  if (!state.initialized || !Number.isFinite(state.value)) {
    state.value = target;
    state.velocity = 0;
    state.initialized = true;
    return state.value;
  }
  const stiffness = 100;
  const damping = 21 * 1.13;
  const mass = 1.12;
  const restDelta = 0.0005;
  const restSpeed = 0.015;
  if (Math.abs(target - state.value) <= restDelta && Math.abs(state.velocity) <= restSpeed) {
    state.value = target;
    state.velocity = 0;
    return state.value;
  }
  const undampedFrequency = Math.sqrt(stiffness / mass);
  const dampingRatio = damping / (2 * Math.sqrt(stiffness * mass));
  const initialDelta = target - state.value;
  const initialVelocity = -state.velocity;
  const seconds = safeDeltaMs / 1_000;
  const current = resolvePreviewSpringPosition(
    seconds,
    target,
    initialDelta,
    initialVelocity,
    dampingRatio,
    undampedFrequency,
  );
  if (
    dampingRatio >= 1
    && ((state.value <= target && current > target) || (state.value >= target && current < target))
  ) {
    state.value = target;
    state.velocity = 0;
    return state.value;
  }
  const epsilon = 0.0001;
  const ahead = resolvePreviewSpringPosition(
    seconds + epsilon,
    target,
    initialDelta,
    initialVelocity,
    dampingRatio,
    undampedFrequency,
  );
  const velocity = (ahead - current) / epsilon;
  if (Math.abs(velocity) <= restSpeed && Math.abs(target - current) <= restDelta) {
    state.value = target;
    state.velocity = 0;
  } else {
    state.value = current;
    state.velocity = velocity;
  }
  return state.value;
};
const buildPreviewCameraTrack = (
  events: PreviewZoomEvent[],
  durationMs: number,
  fps = 120,
): PreviewCameraFrame[] => {
  if (events.length === 0) return [{ timeMs: 0, camera: identityCamera() }];
  const frames: PreviewCameraFrame[] = [];
  const frameDurationMs = 1_000 / fps;
  const groups: Array<{ startMs: number; endMs: number }> = [];
  for (const event of events) {
    const startMs = Math.max(0, event.startMs - frameDurationMs * 2);
    const endMs = Math.min(durationMs, event.endMs + 1_500);
    const previous = groups[groups.length - 1];
    if (previous && startMs <= previous.endMs) previous.endMs = Math.max(previous.endMs, endMs);
    else groups.push({ startMs, endMs });
  }
  for (const group of groups) {
    const scale = createPreviewSpring(1);
    const cropX = createPreviewSpring(0);
    const cropY = createPreviewSpring(0);
    const frameCount = Math.ceil((group.endMs - group.startMs) * fps / 1_000) + 1;
    let previousTimeMs: number | null = null;
    for (let frameIndex = 0; frameIndex < frameCount; frameIndex += 1) {
      const timeMs = Math.min(group.endMs, group.startMs + frameIndex * frameDurationMs);
      const target = previewCameraAt(events, timeMs);
      const deltaMs = previousTimeMs === null ? frameDurationMs : timeMs - previousTimeMs;
      previousTimeMs = timeMs;
      frames.push({
        timeMs,
        camera: {
          scale: stepPreviewSpring(scale, target.scale, deltaMs),
          cropX: stepPreviewSpring(cropX, target.cropX, deltaMs),
          cropY: stepPreviewSpring(cropY, target.cropY, deltaMs),
          hideCursor: target.hideCursor,
        },
      });
    }
  }
  return frames;
};
const cameraFromPreviewTrack = (frames: PreviewCameraFrame[], timeMs: number): PreviewCamera => {
  if (frames.length === 0) return identityCamera();
  if (timeMs < frames[0].timeMs || timeMs >= frames[frames.length - 1].timeMs) return identityCamera();
  let low = 0;
  let high = frames.length - 1;
  while (low < high) {
    const middle = Math.ceil((low + high) / 2);
    if (frames[middle].timeMs <= timeMs) low = middle;
    else high = middle - 1;
  }
  const leftIndex = low;
  const rightIndex = Math.min(frames.length - 1, leftIndex + 1);
  const left = frames[leftIndex];
  const right = frames[rightIndex];
  if (right.timeMs - left.timeMs > 50) return identityCamera();
  const span = Math.max(1, right.timeMs - left.timeMs);
  const progress = Math.max(0, Math.min(1, (timeMs - left.timeMs) / span));
  return {
    scale: left.camera.scale + (right.camera.scale - left.camera.scale) * progress,
    cropX: left.camera.cropX + (right.camera.cropX - left.camera.cropX) * progress,
    cropY: left.camera.cropY + (right.camera.cropY - left.camera.cropY) * progress,
    hideCursor: left.camera.hideCursor,
  };
};

const backgroundPreview = (background: RecordingSettings["background"]) => {
  const preset = backgroundPresets.find(([value]) => value === background);
  if (!preset) return { image: "", color: "#10131a" };
  const [, , file, color] = preset;
  return {
    image: file ? new URL(`../src-tauri/assets/wallpapers/${file}`, import.meta.url).href : "",
    color: color ?? "#10131a",
  };
};

const tahoePreviewCursor = (kind: string) => {
  const known: Record<string, [string, number, number]> = {
    Arrow: [tahoeCursorUrl, 14, 6],
    PointingHand: [new URL("../src-tauri/assets/tahoe/pointinghand-1__40-10.svg", import.meta.url).href, 40, 10],
    Text: [new URL("../src-tauri/assets/tahoe/ibeam-1__50-44.svg", import.meta.url).href, 50, 44],
    VerticalText: [new URL("../src-tauri/assets/tahoe/ibeamvertical-1__50-50.svg", import.meta.url).href, 50, 50],
    OpenHand: [new URL("../src-tauri/assets/tahoe/openhand-1__55-57.svg", import.meta.url).href, 55, 57],
    ClosedHand: [new URL("../src-tauri/assets/tahoe/closedhand-1__50-46.svg", import.meta.url).href, 50, 46],
    Crosshair: [new URL("../src-tauri/assets/tahoe/crosshair-1__50-50.svg", import.meta.url).href, 50, 50],
    NotAllowed: [new URL("../src-tauri/assets/tahoe/notallowed-1__23-0.svg", import.meta.url).href, 23, 0],
    Busy: [new URL("../src-tauri/assets/tahoe/beachball-1__50-46.svg", import.meta.url).href, 50, 46],
    Move: [new URL("../src-tauri/assets/tahoe/move-1__50-46.svg", import.meta.url).href, 50, 46],
    ResizeEastWest: [new URL("../src-tauri/assets/tahoe/resizeeastwest-1__50-50.svg", import.meta.url).href, 50, 50],
    ResizeNorthSouth: [new URL("../src-tauri/assets/tahoe/resizenorthsouth-1__50-49.svg", import.meta.url).href, 50, 49],
    ZoomIn: [new URL("../src-tauri/assets/tahoe/zoomin-1__42-39.svg", import.meta.url).href, 42, 39],
    ZoomOut: [new URL("../src-tauri/assets/tahoe/zoomout-1__43-39.svg", import.meta.url).href, 43, 39],
  };
  const [url, hotspotX, hotspotY] = known[kind] ?? known.Arrow;
  return { url, hotspotX, hotspotY };
};

const cursorSampleAt = (samples: CursorSample[], timeMs: number) => {
  if (samples.length === 0) return null;
  let low = 0;
  let high = samples.length - 1;
  while (low < high) {
    const middle = Math.ceil((low + high) / 2);
    if (samples[middle].timeMs <= timeMs) low = middle;
    else high = middle - 1;
  }
  const current = samples[low];
  const next = samples[Math.min(samples.length - 1, low + 1)];
  if (next.timeMs <= current.timeMs || timeMs <= current.timeMs) return current;
  const progress = Math.max(0, Math.min(1, (timeMs - current.timeMs) / (next.timeMs - current.timeMs)));
  return {
    ...current,
    x: current.x + (next.x - current.x) * progress,
    y: current.y + (next.y - current.y) * progress,
  };
};

const clickSamplesAt = (samples: ClickSample[], timeMs: number, lifetimeMs = 520) => {
  if (samples.length === 0) return [];
  const windowStart = timeMs - lifetimeMs;
  let low = 0;
  let high = samples.length;
  while (low < high) {
    const middle = Math.floor((low + high) / 2);
    if (samples[middle].timeMs < windowStart) low = middle + 1;
    else high = middle;
  }
  const active: ClickSample[] = [];
  for (let index = low; index < samples.length; index += 1) {
    const sample = samples[index];
    if (sample.timeMs > timeMs) break;
    active.push(sample);
  }
  return active;
};

async function setupEditor() {
  document.querySelector<HTMLElement>("#editor-shell")?.removeAttribute("hidden");
  const sessionId = new URLSearchParams(window.location.search).get("editor");
  if (!sessionId) return;
  const query = <T extends Element>(selector: string) => {
    const element = document.querySelector<T>(selector);
    if (!element) throw new Error(`Missing editor element: ${selector}`);
    return element;
  };
  const video = query<HTMLVideoElement>("#editor-video");
  const audioPreview = query<HTMLAudioElement>("#editor-audio-preview");
  const webcamPreview = query<HTMLVideoElement>("#editor-webcam-preview");
  const sceneViewport = query<HTMLElement>("#editor-scene-viewport");
  const sceneCanvas = query<HTMLElement>("#editor-scene-canvas");
  const cameraFrame = query<HTMLElement>("#editor-camera-frame");
  const scrubFrame = query<HTMLCanvasElement>("#editor-scrub-frame");
  const liveCursor = query<HTMLImageElement>("#editor-live-cursor");
  const clickLayer = query<HTMLElement>("#editor-click-layer");
  const videoStatus = query<HTMLElement>("#editor-video-status");
  const durationOutput = query<HTMLOutputElement>("#editor-duration");
  const playhead = query<HTMLInputElement>("#editor-playhead");
  const playheadOutput = query<HTMLOutputElement>("#editor-playhead-output");
  const playheadLine = query<HTMLElement>("#editor-playhead-line");
  const markerLane = query<HTMLElement>("#editor-marker-lane");
  const timelineGrid = query<HTMLElement>(".timeline-grid");
  const clipLane = query<HTMLElement>("#editor-clip-lane");
  const splitClip = query<HTMLButtonElement>("#editor-split");
  const clipMenu = query<HTMLElement>("#editor-clip-menu");
  const deleteClip = query<HTMLButtonElement>("#editor-delete-clip");
  const audioTrackLabel = query<HTMLElement>("#editor-audio-track-label");
  const audioLane = query<HTMLElement>("#editor-audio-lane");
  const rulerLabels = query<HTMLElement>("#editor-ruler-labels");
  const focusPreview = query<HTMLElement>("#editor-focus-preview");
  const selectionTitle = query<HTMLElement>("#editor-selection-title");
  const markerFields = query<HTMLElement>("#editor-marker-fields");
  const markerDepth = query<HTMLInputElement>("#editor-marker-depth");
  const markerDepthOutput = query<HTMLOutputElement>("#editor-marker-depth-output");
  const markerX = query<HTMLInputElement>("#editor-marker-x");
  const markerXOutput = query<HTMLOutputElement>("#editor-marker-x-output");
  const markerY = query<HTMLInputElement>("#editor-marker-y");
  const markerYOutput = query<HTMLOutputElement>("#editor-marker-y-output");
  const markerDurationInput = query<HTMLInputElement>("#editor-marker-duration");
  const markerHideCursor = query<HTMLInputElement>("#editor-marker-hide-cursor");
  const addMarker = query<HTMLButtonElement>("#editor-add-marker");
  const addOrangeMarker = query<HTMLButtonElement>("#editor-add-orange-marker");
  const addMarkerToolbar = query<HTMLButtonElement>("#editor-add-marker-toolbar");
  const deleteMarker = query<HTMLButtonElement>("#editor-delete-marker");
  const undo = query<HTMLButtonElement>("#editor-undo");
  const redo = query<HTMLButtonElement>("#editor-redo");
  const exportButton = query<HTMLButtonElement>("#editor-export");
  const exportMenu = query<HTMLElement>("#editor-export-menu");
  const save = query<HTMLButtonElement>("#editor-save");
  const discard = query<HTMLButtonElement>("#editor-discard");
  const saveState = query<HTMLElement>("#editor-save-state");
  const renderOverlay = query<HTMLElement>("#editor-render-overlay");
  const renderStatus = query<HTMLElement>("#editor-render-status");
  const progress = query<HTMLProgressElement>("#editor-progress");
  const renderPercent = query<HTMLOutputElement>("#editor-render-percent");
  const play = query<HTMLButtonElement>("#editor-play");
  const skipBack = query<HTMLButtonElement>("#editor-skip-back");
  const skipForward = query<HTMLButtonElement>("#editor-skip-forward");
  const volume = query<HTMLButtonElement>("#editor-volume");
  const previewVolume = query<HTMLInputElement>("#editor-preview-volume");
  const background = query<HTMLSelectElement>("#editor-background");
  const customBackground = query<HTMLButtonElement>("#editor-custom-background");
  const customBackgroundName = query<HTMLElement>("#editor-custom-background-name");
  const backgroundEnabled = query<HTMLInputElement>("#editor-background-enabled");
  const audioDetached = query<HTMLInputElement>("#editor-audio-detached");
  const cursorVisible = query<HTMLInputElement>("#editor-cursor-visible");
  const cursorSize = query<HTMLInputElement>("#editor-cursor-size");
  const cursorSizeOutput = query<HTMLOutputElement>("#editor-cursor-size-output");
  const cursorSmoothing = query<HTMLInputElement>("#editor-cursor-smoothing");
  const cursorSmoothingOutput = query<HTMLOutputElement>("#editor-cursor-smoothing-output");
  const clickEffects = query<HTMLInputElement>("#editor-click-effects");
  const editorSystemAudioEnabled = query<HTMLInputElement>("#editor-system-audio-enabled");
  const editorMicrophoneEnabled = query<HTMLInputElement>("#editor-microphone-enabled");
  const editorSystemAudioVolume = query<HTMLInputElement>("#editor-system-audio-volume");
  const editorMicrophoneVolume = query<HTMLInputElement>("#editor-microphone-volume");
  const editorSystemAudioVolumeOutput = query<HTMLOutputElement>("#editor-system-audio-volume-output");
  const editorMicrophoneVolumeOutput = query<HTMLOutputElement>("#editor-microphone-volume-output");
  const customCursor = query<HTMLButtonElement>("#editor-custom-cursor");
  const customCursorName = query<HTMLElement>("#editor-custom-cursor-name");
  const webcamEnabled = query<HTMLInputElement>("#editor-webcam-enabled");
  const webcamMirror = query<HTMLInputElement>("#editor-webcam-mirror");
  const webcamSize = query<HTMLInputElement>("#editor-webcam-size");
  const webcamSizeOutput = query<HTMLOutputElement>("#editor-webcam-size-output");
  const webcamRoundness = query<HTMLInputElement>("#editor-webcam-roundness");
  const webcamRoundnessOutput = query<HTMLOutputElement>("#editor-webcam-roundness-output");
  const webcamShadow = query<HTMLInputElement>("#editor-webcam-shadow");
  const webcamShadowOutput = query<HTMLOutputElement>("#editor-webcam-shadow-output");
  const webcamReact = query<HTMLInputElement>("#editor-webcam-react");
  const webcamAvailability = query<HTMLElement>("#editor-webcam-availability");
  const aspectLabel = query<HTMLElement>("#editor-aspect-label");
  const exportFps = query<HTMLElement>("#editor-export-fps");
  const exportQuality = query<HTMLElement>("#editor-export-quality");
  const previewColumn = query<HTMLElement>(".editor-preview-column");
  const previewStage = query<HTMLElement>(".recordly-preview-stage");
  const toolButtons = [...document.querySelectorAll<HTMLButtonElement>("[data-editor-tool]")];
  const toolPanels = [...document.querySelectorAll<HTMLElement>("[data-editor-panel]")];
  const editorBackgroundGallery = document.querySelector<HTMLElement>('[data-background-gallery="editor"]');

  const clone = <T>(value: T): T => JSON.parse(JSON.stringify(value)) as T;
  let session: EditorSession;
  let sessionReady = false;
  let selectedMarker: number | null = null;
  let selectedSegment: number | null = null;
  let busy = false;
  let syncEditorBackgroundGallery = () => {};
  let liveFrameRequest = 0;
  let videoFrameRequest = 0;
  let smoothedPreviewCursor: { x: number; y: number } | null = null;
  let smoothedPreviewCursorTime = -1;
  let smoothedPreviewCursorAmount = -1;
  let previewCameraTrack: PreviewCameraFrame[] = [];
  let previewCameraTrackDirty = true;
  let previewFrameHeight = 0;
  let lastSidecarSyncAt = 0;
  let hiddenAt: number | null = null;
  let previewNeedsRefresh = false;
  let previewRefresh: Promise<void> | null = null;
  let editorDisposed = false;
  let previewVolumeLevel = 1;
  let stopProcessingProgress: (() => void) | null = null;
  const clickPulsePool: HTMLSpanElement[] = [];
  const publishedAssets = new Map<string, string>();
  const history: EditorSnapshot[] = [];
  const future: EditorSnapshot[] = [];

  const layoutPreviewCanvas = () => {
    if (!sessionReady || !session.width || !session.height) return;
    const bounds = previewColumn.getBoundingClientRect();
    if (bounds.width <= 0 || bounds.height <= 0) return;
    const availableWidth = Math.max(1, bounds.width - 28);
    const availableHeight = Math.max(1, Math.min(bounds.height - 124, window.innerHeight * 0.60, 620));
    const aspect = session.width / session.height;
    let width = availableWidth;
    let height = width / aspect;
    if (height > availableHeight) {
      height = availableHeight;
      width = height * aspect;
    }
    const pixelWidth = Math.max(1, Math.floor(width));
    const pixelHeight = Math.max(1, Math.floor(height));
    previewStage.style.width = `${pixelWidth}px`;
    previewStage.style.height = `${pixelHeight}px`;
    sceneViewport.style.width = `${pixelWidth}px`;
    sceneViewport.style.height = `${pixelHeight}px`;
    previewFrameHeight = Math.max(1, Math.floor(pixelHeight * 0.92));
  };
  const previewAssetUrl = (path: string | null, fallback: string | null) =>
    path ? publishedAssets.get(path) ?? fallback : null;
  const syncSidecarTime = (force = false) => {
    const now = performance.now();
    if (!force && now - lastSidecarSyncAt < 500) return;
    lastSidecarSyncAt = now;
    for (const sidecar of [audioPreview, webcamPreview]) {
      if (!sidecar.src || !Number.isFinite(video.currentTime)) continue;
      const drift = sidecar.currentTime - video.currentTime;
      if (force || Math.abs(drift) > 0.45) {
        sidecar.currentTime = video.currentTime;
        sidecar.playbackRate = 1;
      } else if (!video.paused && Math.abs(drift) > 0.06) {
        sidecar.playbackRate = Math.max(0.97, Math.min(1.03, 1 - drift * 0.08));
      } else if (Math.abs(sidecar.playbackRate - 1) > 0.001) {
        sidecar.playbackRate = 1;
      }
    }
  };
  const cameraAtEditorTime = (timeMs: number) => {
    if (previewCameraTrackDirty) {
      previewCameraTrack = buildPreviewCameraTrack(
        buildPreviewZoomEvents(session.markers, session.settings.zoomScale),
        session.durationMs,
      );
      previewCameraTrackDirty = false;
    }
    return cameraFromPreviewTrack(previewCameraTrack, timeMs);
  };
  const invalidatePreviewCameraTrack = () => {
    previewCameraTrackDirty = true;
  };
  const syncStaticPreview = () => {
    if (!sessionReady) return;
    video.style.width = "100%";
    video.style.height = "100%";
    video.style.left = "0";
    video.style.top = "0";

    // With the scene off the recording is the whole frame, so there is nothing
    // to put behind it. The stylesheet drops the card's inset and rounding off
    // the same flag, matching what the export produces.
    const sceneOn = session.settings.backgroundEnabled !== false;
    sceneCanvas.dataset.scene = sceneOn ? "on" : "off";
    const customBackgroundUrl = sceneOn
      ? previewAssetUrl(session.settings.customBackgroundPath, session.customBackgroundUrl)
      : "";
    const selectedBackground = backgroundPreview(session.settings.background);
    sceneCanvas.style.backgroundImage = !sceneOn
      ? "none"
      : customBackgroundUrl
        ? `url("${customBackgroundUrl}")`
        : selectedBackground.image
          ? `url("${selectedBackground.image}")`
          : "none";
    sceneCanvas.style.backgroundColor = !sceneOn
      ? "#000"
      : customBackgroundUrl
        ? "#10131a"
        : selectedBackground.color;

    const showWebcam = Boolean(session.hasWebcam && session.webcamUrl && session.settings.webcam.enabled);
    webcamPreview.hidden = !showWebcam;
    if (showWebcam) {
      const webcam = session.settings.webcam;
      webcamPreview.style.width = `${Math.max(10, Math.min(100, webcam.sizePercent))}%`;
      webcamPreview.style.right = `${webcam.margin}px`;
      webcamPreview.style.bottom = `${webcam.margin}px`;
      webcamPreview.style.borderRadius = `${webcam.roundness}px`;
      webcamPreview.style.boxShadow = `0 14px 34px rgb(0 0 0 / ${0.48 * webcam.shadow})`;
      webcamPreview.style.transform = webcam.mirror ? "scaleX(-1)" : "none";
    }
  };
  const updateLivePreview = (timeMs = Math.round(video.currentTime * 1000 || Number(playhead.value || 0))) => {
    if (!sessionReady) return;
    const camera = cameraAtEditorTime(timeMs);
    // Mirrors recordly_padded_size: inset card while the scene is on, whole
    // frame once it is off.
    const sceneOn = session.settings.backgroundEnabled !== false;
    const contentInset = sceneOn ? 0.04 : 0;
    const contentScale = sceneOn ? 0.92 : 1;
    const viewportSize = 1 / camera.scale;
    const sourceFocusX = camera.cropX + viewportSize / 2;
    const sourceFocusY = camera.cropY + viewportSize / 2;
    const sceneCropX = Math.max(
      0,
      Math.min(1 - viewportSize, contentInset + sourceFocusX * contentScale - viewportSize / 2),
    );
    const sceneCropY = Math.max(
      0,
      Math.min(1 - viewportSize, contentInset + sourceFocusY * contentScale - viewportSize / 2),
    );
    const cameraX = -sceneCropX * camera.scale * 100;
    const cameraY = -sceneCropY * camera.scale * 100;
    sceneCanvas.style.transform =
      `translate3d(${cameraX}%, ${cameraY}%, 0) scale(${camera.scale})`;

    const rawCursorSample = session.syntheticCursor
      ? cursorSampleAt(session.cursorSamples, timeMs)
      : null;
    let cursorSample = rawCursorSample;
    if (rawCursorSample) {
      const smoothing = session.settings.cursorSmoothing;
      const deltaMs = timeMs - smoothedPreviewCursorTime;
      if (
        !smoothedPreviewCursor
        || smoothing === 0
        || smoothing !== smoothedPreviewCursorAmount
        || deltaMs <= 0
        || deltaMs > 150
      ) {
        smoothedPreviewCursor = { x: rawCursorSample.x, y: rawCursorSample.y };
      } else {
        const responseMs = 12 + smoothing * 1.45;
        const blend = 1 - Math.exp(-deltaMs / responseMs);
        smoothedPreviewCursor.x += (rawCursorSample.x - smoothedPreviewCursor.x) * blend;
        smoothedPreviewCursor.y += (rawCursorSample.y - smoothedPreviewCursor.y) * blend;
      }
      smoothedPreviewCursorTime = timeMs;
      smoothedPreviewCursorAmount = smoothing;
      cursorSample = { ...rawCursorSample, ...smoothedPreviewCursor };
    }
    const cursorVisibleAtFrame = Boolean(
      cursorSample
      && session.settings.cursorVisible
      && !camera.hideCursor
      && cursorSample.kind !== "Hidden",
    );
    if (cursorSample && cursorVisibleAtFrame) {
      const customCursorUrl = previewAssetUrl(session.settings.customCursorPath, session.customCursorUrl);
      const cursorAsset = customCursorUrl
        ? { url: customCursorUrl, hotspotX: 0, hotspotY: 0 }
        : tahoePreviewCursor(cursorSample.kind);
      if (liveCursor.src !== cursorAsset.url) liveCursor.src = cursorAsset.url;
      liveCursor.style.left = `${cursorSample.x * 100}%`;
      liveCursor.style.top = `${cursorSample.y * 100}%`;
      liveCursor.style.height = `${Math.max(18, previewFrameHeight * 0.075 * session.settings.cursorSize / 100)}px`;
      liveCursor.style.transform = `translate(-${cursorAsset.hotspotX}%, -${cursorAsset.hotspotY}%)`;
      liveCursor.style.opacity = "1";
    } else {
      liveCursor.style.opacity = "0";
    }

    const activeClicks = session.settings.clickEffectsEnabled
      ? clickSamplesAt(session.clickSamples, timeMs)
      : [];
    while (clickPulsePool.length < activeClicks.length) {
      const pulse = document.createElement("span");
      clickPulsePool.push(pulse);
      clickLayer.append(pulse);
    }
    clickPulsePool.forEach((pulse, index) => {
      const sample = activeClicks[index];
      if (!sample) {
        pulse.hidden = true;
        return;
      }
      pulse.hidden = false;
      pulse.className = `editor-click-pulse${sample.primary ? "" : " secondary"}`;
      const progress = Math.max(0, Math.min(1, (timeMs - sample.timeMs) / 520));
      pulse.style.left = `${sample.x * 100}%`;
      pulse.style.top = `${sample.y * 100}%`;
      pulse.style.opacity = String((1 - progress) * 0.92);
      pulse.style.transform = `translate(-50%, -50%) scale(${(0.38 + progress * 1.35) * camera.scale})`;
    });
  };
  const stopLiveFrames = () => {
    if (liveFrameRequest) cancelAnimationFrame(liveFrameRequest);
    if (videoFrameRequest && typeof video.cancelVideoFrameCallback === "function") {
      video.cancelVideoFrameCallback(videoFrameRequest);
    }
    liveFrameRequest = 0;
    videoFrameRequest = 0;
  };
  const runLiveFrames = () => {
    stopLiveFrames();
    const frame = () => {
      if (editorDisposed || video.paused || video.ended || document.hidden) {
        stopLiveFrames();
        return;
      }
      const atMs = video.currentTime * 1000;
      const current = session.segments.findIndex(
        (segment) => atMs >= segment.startMs && atMs < segment.endMs,
      );
      if (current < 0 || atMs >= session.segments[current].endMs) {
        // Past the end of a piece: continue at the next one, or stop if this
        // was the last.
        const next = session.segments.find((segment) => segment.startMs > atMs);
        if (next) {
          video.currentTime = next.startMs / 1000;
          syncSidecarTime(true);
        } else {
          video.pause();
          video.currentTime = session.trimEndMs / 1000;
          return;
        }
      }
      playhead.value = String(Math.round(video.currentTime * 1000));
      updatePlayhead();
      updateLivePreview();
      syncSidecarTime();
      if (typeof video.requestVideoFrameCallback === "function") {
        videoFrameRequest = video.requestVideoFrameCallback(() => {
          videoFrameRequest = 0;
          frame();
        });
      } else {
        liveFrameRequest = requestAnimationFrame(() => {
          liveFrameRequest = 0;
          frame();
        });
      }
    };
    frame();
  };
  const previewResizeObserver = new ResizeObserver(() => {
    layoutPreviewCanvas();
    updateLivePreview();
  });
  previewResizeObserver.observe(previewColumn);
  const timelineResizeObserver = new ResizeObserver(() => syncTimeline());
  timelineResizeObserver.observe(timelineGrid);
  const selectSegment = (index: number | null) => {
    selectedSegment = index;
    syncTimeline();
  };
  // The trim fields still describe the outer edges of what survives, which is
  // what the rest of the pipeline and older payloads read.
  const syncTrimToSegments = () => {
    const first = session.segments[0];
    const last = session.segments[session.segments.length - 1];
    session.trimStartMs = first.startMs;
    session.trimEndMs = last.endMs;
  };
  const segmentAt = (timeMs: number) =>
    session.segments.findIndex(
      (segment) => timeMs > segment.startMs && timeMs < segment.endMs,
    );
  const cutAtPlayhead = () => {
    const at = Math.round(Number(playhead.value));
    const index = segmentAt(at);
    // Cutting exactly on an edge would make an empty piece.
    if (index < 0) return;
    mutate(() => {
      const segment = session.segments[index];
      session.segments.splice(index, 1, { startMs: segment.startMs, endMs: at }, {
        startMs: at,
        endMs: segment.endMs,
      });
      syncTrimToSegments();
    });
    selectSegment(index + 1);
  };
  const deleteSelectedSegment = () => {
    // Something has to survive, so the last piece cannot be removed.
    if (selectedSegment === null || session.segments.length < 2) return;
    const index = selectedSegment;
    mutate(() => {
      session.segments.splice(index, 1);
      syncTrimToSegments();
    });
    selectSegment(null);
    // The playhead may now sit in the gap the cut left behind.
    const settled = clampToTrim(Number(playhead.value));
    playhead.value = String(settled);
    seekPreview(settled);
    updatePlayhead();
  };
  let copiedSegment: Segment | null = null;
  // Where a right-click landed, so the menu acts on that piece rather than on
  // whatever happened to be selected.
  let menuTimeMs = 0;

  const canPaste = () => {
    if (!copiedSegment) return false;
    // Pasting puts footage back into a gap the cuts left. Anything else would
    // have to overlap a piece that is already there.
    return !session.segments.some(
      (segment) =>
        copiedSegment!.startMs < segment.endMs && segment.startMs < copiedSegment!.endMs,
    );
  };
  const copySelectedSegment = () => {
    if (selectedSegment === null) return;
    copiedSegment = { ...session.segments[selectedSegment] };
  };
  const pasteSegment = () => {
    if (!copiedSegment || !canPaste()) return;
    const restored = { ...copiedSegment };
    mutate(() => {
      session.segments.push(restored);
      session.segments.sort((left, right) => left.startMs - right.startMs);
      syncTrimToSegments();
    });
    selectSegment(session.segments.findIndex((segment) => segment.startMs === restored.startMs));
  };

  const closeClipMenu = () => {
    clipMenu.hidden = true;
  };
  const openClipMenu = (event: MouseEvent, overZoomLane: boolean) => {
    const rect = clipLane.getBoundingClientRect();
    if (rect.width <= 0) return;
    const ratio = (event.clientX - rect.left) / rect.width;
    menuTimeMs = Math.max(0, Math.min(session.durationMs, ratio * session.durationMs));
    const under = session.segments.findIndex(
      (segment) => menuTimeMs >= segment.startMs && menuTimeMs <= segment.endMs,
    );
    selectSegment(under < 0 ? null : under);

    // Adding a zoom belongs to the zoom lane; cutting belongs to the clip lane.
    clipMenu
      .querySelectorAll<HTMLElement>("[data-clip-group]")
      .forEach((group) => {
        group.hidden = group.dataset.clipGroup === "zoom" ? !overZoomLane : overZoomLane;
      });
    clipMenu.querySelectorAll<HTMLButtonElement>("[data-clip-action]").forEach((item) => {
      const action = item.dataset.clipAction;
      if (action === "split") item.disabled = segmentAt(menuTimeMs) < 0;
      if (action === "copy") item.disabled = selectedSegment === null;
      if (action === "paste") item.disabled = !canPaste();
      if (action === "delete") {
        item.disabled = selectedSegment === null || session.segments.length < 2;
      }
    });

    clipMenu.hidden = false;
    // Measure once visible so a menu near the edge folds back into view.
    const menuRect = clipMenu.getBoundingClientRect();
    const left = Math.min(event.clientX, window.innerWidth - menuRect.width - 8);
    const top = Math.min(event.clientY, window.innerHeight - menuRect.height - 8);
    clipMenu.style.left = `${Math.max(8, left)}px`;
    clipMenu.style.top = `${Math.max(8, top)}px`;
  };

  const snapshot = (): EditorSnapshot => clone({
    trimStartMs: session.trimStartMs,
    trimEndMs: session.trimEndMs,
    segments: session.segments,
    audioSegments: session.audioSegments,
    markers: session.markers,
    settings: session.settings,
  });
  const pushHistory = () => {
    history.push(snapshot());
    if (history.length > 80) history.shift();
    future.length = 0;
  };
  const markDirty = () => {
    saveState.textContent = "Edited";
    undo.disabled = busy || history.length === 0;
    redo.disabled = busy || future.length === 0;
  };
  const applySnapshot = (next: EditorSnapshot) => {
    session.trimStartMs = next.trimStartMs;
    session.trimEndMs = next.trimEndMs;
    session.segments = clone(next.segments);
    session.audioSegments = clone(next.audioSegments ?? []);
    selectedSegment = null;
    session.markers = clone(next.markers);
    session.settings = clone(next.settings);
    invalidatePreviewCameraTrack();
    selectedMarker = selectedMarker === null || session.markers.length === 0
      ? null
      : Math.min(selectedMarker, session.markers.length - 1);
    markDirty();
    syncAll();
  };
  const setBusy = (next: boolean) => {
    busy = next;
    renderOverlay.hidden = !next;
    save.disabled = next;
    discard.disabled = next;
    exportButton.disabled = next;
    addMarker.disabled = next;
    addOrangeMarker.disabled = next;
    addMarkerToolbar.disabled = next;
    deleteMarker.disabled = next || selectedMarker === null;
    undo.disabled = next || history.length === 0;
    redo.disabled = next || future.length === 0;
  };
  const activateTool = (tool: string) => {
    toolButtons.forEach((button) => button.classList.toggle("active", button.dataset.editorTool === tool));
    toolPanels.forEach((panel) => panel.classList.toggle("active", panel.dataset.editorPanel === tool));
  };
  toolButtons.forEach((button) => button.addEventListener("click", () => activateTool(button.dataset.editorTool ?? "scene")));

  const markerTotalDuration = (marker: ZoomMarker) => Math.max(
    previewZoomTiming.inMs + previewZoomTiming.outMs,
    marker.heldUntilMs === null
      ? 3_000
      : marker.heldUntilMs - marker.timeMs + previewZoomTiming.outMs,
  );
  const setMarkerDuration = (marker: ZoomMarker, requestedMs: number) => {
    const total = Math.max(1_000, Math.min(requestedMs, session.durationMs - marker.timeMs));
    marker.heldUntilMs = Math.abs(total - 3_000) < 80
      ? null
      : marker.timeMs + Math.max(previewZoomTiming.inMs, total - previewZoomTiming.outMs);
  };
  const selected = () => selectedMarker === null ? null : session.markers[selectedMarker] ?? null;
  const selectMarker = (marker: ZoomMarker | null) => {
    selectedMarker = marker ? session.markers.indexOf(marker) : null;
    if (marker) {
      playhead.value = String(marker.timeMs);
      video.currentTime = marker.timeMs / 1000;
      syncSidecarTime(true);
      updateLivePreview(marker.timeMs);
      activateTool("zoom");
    }
    syncMarkerPanel();
    syncTimeline();
  };
  const syncPreviewVolume = () => {
    const level = Math.max(0, Math.min(1, previewVolumeLevel));
    video.volume = level;
    audioPreview.volume = level;
    volume.classList.toggle("muted", video.muted || level === 0);
    volume.setAttribute("aria-label", video.muted || level === 0 ? "Unmute preview" : "Mute preview");
  };
  const syncMarkerPanel = () => {
    const marker = selected();
    markerFields.hidden = !marker;
    deleteMarker.disabled = busy || !marker;
    selectionTitle.textContent = marker ? `Zoom ${selectedMarker! + 1}` : "Select a zoom region";
    focusPreview.hidden = !marker;
    if (!marker) return;
    const depth = marker.targetScale ?? session.settings.zoomScale;
    markerDepth.value = String(Math.round(depth * 100));
    markerDepthOutput.value = `${depth.toFixed(2).replace(/0$/, "")}×`;
    markerX.value = String(Math.round(marker.x * 100));
    markerXOutput.value = `${markerX.value}%`;
    markerY.value = String(Math.round(marker.y * 100));
    markerYOutput.value = `${markerY.value}%`;
    markerDurationInput.value = (markerTotalDuration(marker) / 1000).toFixed(1);
    markerHideCursor.checked = marker.hideCursor;
    focusPreview.style.left = `${marker.x * 100}%`;
    focusPreview.style.top = `${marker.y * 100}%`;
    focusPreview.style.width = `${Math.min(82, 100 / depth)}%`;
  };
  // Everything outside the trim is dropped on export, and on this recording it
  // is the black lead in and lead out. Keeping the playhead inside the trim is
  // what makes the editor show the same thing the export produces.
  // Land inside what survives. Everything between the cuts was removed, so a
  // time that falls in a gap belongs at the start of the next piece.
  const clampToTrim = (ms: number) => {
    const segments = session.segments;
    if (!segments.length) return ms;
    if (ms <= segments[0].startMs) return segments[0].startMs;
    for (const segment of segments) {
      if (ms < segment.startMs) return segment.startMs;
      if (ms <= segment.endMs) return ms;
    }
    return segments[segments.length - 1].endMs;
  };
  const updatePlayhead = () => {
    const now = Number(playhead.value);
    const ratio = session.durationMs ? now / session.durationMs : 0;
    playheadOutput.value = formatElapsed(now);
    const laneLeft = markerLane.offsetLeft;
    const laneWidth = markerLane.clientWidth;
    playhead.style.left = `${laneLeft}px`;
    playhead.style.width = `${laneWidth}px`;
    playheadLine.style.left = `${laneLeft + laneWidth * ratio}px`;
  };
  const beginTimelineDrag = (event: PointerEvent, marker: ZoomMarker, mode: "move" | "start" | "end") => {
    event.preventDefault();
    event.stopPropagation();
    pushHistory();
    const rect = markerLane.getBoundingClientRect();
    const startX = event.clientX;
    const originalStart = marker.timeMs;
    const originalDuration = markerTotalDuration(marker);
    const originalEnd = Math.min(session.durationMs, originalStart + originalDuration);
    selectMarker(marker);
    const move = (nextEvent: PointerEvent) => {
      const delta = Math.round((nextEvent.clientX - startX) / rect.width * session.durationMs);
      if (mode === "move") {
        marker.timeMs = Math.max(0, Math.min(session.durationMs - originalDuration, originalStart + delta));
        setMarkerDuration(marker, originalDuration);
      } else if (mode === "start") {
        marker.timeMs = Math.max(0, Math.min(originalEnd - 1_000, originalStart + delta));
        setMarkerDuration(marker, originalEnd - marker.timeMs);
      } else {
        setMarkerDuration(marker, Math.max(1_000, Math.min(session.durationMs, originalEnd + delta) - marker.timeMs));
      }
      invalidatePreviewCameraTrack();
      playhead.value = String(marker.timeMs);
      markDirty();
      syncMarkerPanel();
      syncTimeline();
      updateLivePreview(marker.timeMs);
    };
    const end = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", end);
      session.markers.sort((left, right) => left.timeMs - right.timeMs);
      selectedMarker = session.markers.indexOf(marker);
      syncTimeline();
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", end, { once: true });
  };
  const syncTimeline = () => {
    // A ResizeObserver fires as soon as it starts observing, which is before
    // the session has loaded.
    if (!sessionReady) return;
    const hasAudioTrack = Boolean(session.audioUrl) && session.audioSegments.length > 0;
    audioTrackLabel.hidden = !hasAudioTrack;
    audioLane.hidden = !hasAudioTrack;
    timelineGrid.classList.toggle("audio-present", hasAudioTrack);
    updatePlayhead();
    markerLane.replaceChildren();
    session.markers.forEach((marker, index) => {
      const start = marker.timeMs / session.durationMs * 100;
      const endMs = Math.min(session.durationMs, marker.timeMs + markerTotalDuration(marker));
      const width = Math.max(1.6, (endMs - marker.timeMs) / session.durationMs * 100);
      const region = document.createElement("button");
      region.type = "button";
      region.className = "recordly-zoom-region";
      region.classList.toggle("selected", index === selectedMarker);
      region.classList.toggle("cursor-hidden", marker.hideCursor);
      region.style.left = `${start}%`;
      region.style.width = `${width}%`;
      region.innerHTML = `<i class="recordly-region-handle start"></i><span>Zoom ${index + 1}</span><i class="recordly-region-handle end"></i>`;
      region.title = `${formatElapsed(marker.timeMs)} – ${formatElapsed(endMs)}`;
      region.addEventListener("click", (event) => {
        event.stopPropagation();
        selectMarker(marker);
      });
      region.addEventListener("pointerdown", (event) => {
        const target = event.target as HTMLElement;
        beginTimelineDrag(event, marker, target.classList.contains("start") ? "start" : target.classList.contains("end") ? "end" : "move");
      });
      markerLane.append(region);
    });
    clipLane.replaceChildren(...session.segments.map((segment, index) => {
      const region = document.createElement("button");
      region.type = "button";
      region.className = "clip-region";
      region.style.left = `${(segment.startMs / session.durationMs) * 100}%`;
      region.style.width = `${((segment.endMs - segment.startMs) / session.durationMs) * 100}%`;
      region.classList.toggle("selected", index === selectedSegment);
      region.setAttribute("aria-pressed", String(index === selectedSegment));
      region.title = `${formatElapsed(segment.startMs)} - ${formatElapsed(segment.endMs)}`;
      const start = document.createElement("span");
      start.className = "trim-handle recordly-trim-handle start";
      const label = document.createElement("span");
      label.innerHTML = '<i class="ph ph-monitor-play"></i> Screen recording';
      const end = document.createElement("span");
      end.className = "trim-handle recordly-trim-handle end";
      if (index === 0) bindTrimHandle(start, "start");
      if (index === session.segments.length - 1) bindTrimHandle(end, "end");
      region.append(start, label, end);
      region.dataset.segment = String(index);
      region.addEventListener("click", (event) => {
        if ((event.target as HTMLElement).closest(".trim-handle")) return;
        selectSegment(index === selectedSegment ? null : index);
      });
      return region;
    }));
    deleteClip.disabled = selectedSegment === null || session.segments.length < 2;

    // What the cuts removed is gone from the whole timeline, not just the clip
    // lane, so the other tracks show the same holes rather than implying their
    // contents survive there.
    const gaps: Array<[number, number]> = [];
    let cursor = 0;
    for (const segment of session.segments) {
      if (segment.startMs > cursor) gaps.push([cursor, segment.startMs]);
      cursor = segment.endMs;
    }
    if (cursor < session.durationMs) gaps.push([cursor, session.durationMs]);
    const paintGaps = (lane: HTMLElement, ranges: Array<[number, number]>) => {
      lane.querySelectorAll(".lane-gap").forEach((node) => node.remove());
      for (const [from, to] of ranges) {
        const gap = document.createElement("span");
        gap.className = "lane-gap";
        gap.style.left = `${(from / session.durationMs) * 100}%`;
        gap.style.width = `${((to - from) / session.durationMs) * 100}%`;
        lane.append(gap);
      }
    };
    paintGaps(markerLane, gaps);
    if (hasAudioTrack) {
      // Detached audio keeps its own cuts, so its holes are its own.
      const audioGaps: Array<[number, number]> = [];
      let audioCursor = 0;
      for (const segment of session.audioSegments) {
        if (segment.startMs > audioCursor) audioGaps.push([audioCursor, segment.startMs]);
        audioCursor = segment.endMs;
      }
      if (audioCursor < session.durationMs) audioGaps.push([audioCursor, session.durationMs]);
      paintGaps(audioLane, audioGaps);
    }
  };
  const syncSettings = () => {
    background.value = session.settings.background;
    syncEditorBackgroundGallery();
    customBackgroundName.textContent = session.settings.customBackgroundPath?.split("/").pop() ?? "No custom image";
    backgroundEnabled.checked = session.settings.backgroundEnabled !== false;
    audioDetached.checked = session.audioSegments.length > 0;
    audioDetached.disabled = !session.audioUrl;
    cursorVisible.checked = session.settings.cursorVisible;
    cursorSize.value = String(session.settings.cursorSize);
    cursorSizeOutput.value = `${session.settings.cursorSize}%`;
    cursorSmoothing.value = String(session.settings.cursorSmoothing);
    cursorSmoothingOutput.value = `${session.settings.cursorSmoothing}%`;
    clickEffects.checked = session.settings.clickEffectsEnabled;
    editorSystemAudioEnabled.checked = session.settings.audio.systemEnabled;
    editorMicrophoneEnabled.checked = session.settings.audio.microphoneEnabled;
    editorSystemAudioVolume.value = String(Math.round(session.settings.audio.systemVolume * 100));
    editorMicrophoneVolume.value = String(Math.round(session.settings.audio.microphoneVolume * 100));
    editorSystemAudioVolumeOutput.value = `${editorSystemAudioVolume.value}%`;
    editorMicrophoneVolumeOutput.value = `${editorMicrophoneVolume.value}%`;
    customCursorName.textContent = session.settings.customCursorPath?.split("/").pop() ?? "Tahoe cursor set";
    webcamEnabled.checked = session.settings.webcam.enabled && session.hasWebcam;
    webcamMirror.checked = session.settings.webcam.mirror;
    webcamSize.value = String(session.settings.webcam.sizePercent);
    webcamSizeOutput.value = `${session.settings.webcam.sizePercent}%`;
    webcamRoundness.value = String(session.settings.webcam.roundness);
    webcamRoundnessOutput.value = String(session.settings.webcam.roundness);
    webcamShadow.value = String(Math.round(session.settings.webcam.shadow * 100));
    webcamShadowOutput.value = `${webcamShadow.value}%`;
    webcamReact.checked = session.settings.webcam.reactToZoom;
    exportFps.textContent = `${session.settings.fps} FPS`;
    exportQuality.textContent = session.settings.quality[0].toUpperCase() + session.settings.quality.slice(1);
    syncStaticPreview();
  };
  const syncAll = () => {
    durationOutput.value = formatElapsed(session.durationMs);
    playhead.max = String(session.durationMs);
    playhead.value = String(clampToTrim(Number(playhead.value || 0)));
    syncSettings();
    syncMarkerPanel();
    syncTimeline();
    syncPreviewVolume();
    updateLivePreview();
    syncRangeFills();
  };
  const syncSession = (next: EditorSession, reloadVideo = true) => {
    const resumeAt = Number.isFinite(video.currentTime) ? video.currentTime : 0;
    session = next;
    // A manifest older than cutting arrives without them.
    if (!Array.isArray(session.segments) || session.segments.length === 0) {
      session.segments = [{ startMs: session.trimStartMs, endMs: session.trimEndMs }];
    }
    // Empty means the sound is cut wherever the picture is.
    if (!Array.isArray(session.audioSegments)) session.audioSegments = [];
    selectedSegment = null;
    sessionReady = true;
    aspectLabel.innerHTML = `<i class="ph ph-monitor"></i> ${next.width} × ${next.height}`;
    smoothedPreviewCursor = null;
    smoothedPreviewCursorTime = -1;
    previewCameraTrack = [];
    previewCameraTrackDirty = true;
    previewNeedsRefresh = false;
    if (next.settings.customBackgroundPath && next.customBackgroundUrl) {
      publishedAssets.set(next.settings.customBackgroundPath, next.customBackgroundUrl);
    }
    if (next.settings.customCursorPath && next.customCursorUrl) {
      publishedAssets.set(next.settings.customCursorPath, next.customCursorUrl);
    }
    if (reloadVideo) {
      video.pause();
      video.removeAttribute("src");
      video.load();
      video.src = next.videoUrl;
      video.addEventListener("loadedmetadata", () => {
        video.currentTime = Math.min(resumeAt, video.duration || resumeAt);
        syncSidecarTime(true);
        updateLivePreview();
      }, { once: true });
      video.load();
    }
    if (next.audioUrl) {
      if (audioPreview.src !== next.audioUrl) {
        audioPreview.src = next.audioUrl;
        audioPreview.load();
      }
    } else {
      audioPreview.removeAttribute("src");
      audioPreview.load();
    }
    if (next.webcamUrl) {
      if (webcamPreview.src !== next.webcamUrl) {
        webcamPreview.src = next.webcamUrl;
        webcamPreview.load();
      }
    } else {
      webcamPreview.removeAttribute("src");
      webcamPreview.load();
    }
    layoutPreviewCanvas();
    syncAll();
  };
  const waitForMediaState = (
    media: HTMLMediaElement,
    minimumReadyState: number,
    events: string[],
    timeoutMs = 6_000,
  ) => new Promise<void>((resolve, reject) => {
    if (media.readyState >= minimumReadyState) {
      resolve();
      return;
    }
    let settled = false;
    const cleanup = () => {
      window.clearTimeout(timeout);
      events.forEach((event) => media.removeEventListener(event, ready));
      media.removeEventListener("error", failed);
    };
    const ready = () => {
      if (settled || media.readyState < minimumReadyState) return;
      settled = true;
      cleanup();
      resolve();
    };
    const failed = () => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error(`Media error ${media.error?.code ?? "unknown"}`));
    };
    const timeout = window.setTimeout(() => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error("Preview refresh timed out"));
    }, timeoutMs);
    events.forEach((event) => media.addEventListener(event, ready));
    media.addEventListener("error", failed, { once: true });
  });
  const reloadMediaAt = async (media: HTMLMediaElement, source: string, atSeconds: number) => {
    media.pause();
    media.removeAttribute("src");
    media.load();
    media.src = source;
    media.load();
    await waitForMediaState(media, HTMLMediaElement.HAVE_METADATA, ["loadedmetadata"]);
    media.currentTime = Math.max(0, Math.min(atSeconds, media.duration || atSeconds));
    await waitForMediaState(media, HTMLMediaElement.HAVE_CURRENT_DATA, ["loadeddata", "canplay", "seeked"]);
  };
  const refreshIdlePreview = () => {
    if (previewRefresh) return previewRefresh;
    previewRefresh = (async () => {
      const resumeAt = Number.isFinite(video.currentTime)
        ? video.currentTime
        : Number(playhead.value || session.trimStartMs) / 1000;
      const muted = video.muted;
      videoStatus.textContent = "Refreshing preview…";
      stopLiveFrames();
      audioPreview.pause();
      webcamPreview.pause();
      await reloadMediaAt(video, session.videoUrl, resumeAt);
      const sidecars: Promise<void>[] = [];
      if (session.audioUrl) sidecars.push(reloadMediaAt(audioPreview, session.audioUrl, resumeAt));
      if (session.webcamUrl) sidecars.push(reloadMediaAt(webcamPreview, session.webcamUrl, resumeAt));
      await Promise.allSettled(sidecars);
      video.muted = muted;
      audioPreview.muted = muted;
      playhead.value = String(Math.round(resumeAt * 1000));
      updatePlayhead();
      syncSidecarTime(true);
      updateLivePreview();
      previewNeedsRefresh = false;
      videoStatus.textContent = "Preview";
    })().finally(() => {
      previewRefresh = null;
    });
    return previewRefresh;
  };
  // Hiding the editor lets the webview release the media element's backing
  // resources. What comes back reports readyState HAVE_ENOUGH_DATA while
  // holding no buffered data at all: play() resolves, currentTime never moves
  // and no frame is ever painted, so the preview looks frozen while the sidecar
  // audio keeps going. Nothing about the element says "error", so the empty
  // buffer is the signal that it has to be reloaded.
  // HAVE_CURRENT_DATA upwards means there is data for the current position, so
  // it has to be buffered somewhere. An element still loading has a low
  // readyState and an empty buffer, which is ordinary and left alone.
  // A media element the webview has dropped still claims HAVE_ENOUGH_DATA while
  // holding nothing: play() resolves, currentTime never advances and no frame
  // is painted. Both halves are needed to tell that apart from ordinary work -
  // a seek empties the buffer for a moment too, and treating that as death
  // reloads the whole preview for no reason.
  const previewMediaIsStale = () =>
    Boolean(video.error)
    || (video.readyState >= HTMLMediaElement.HAVE_ENOUGH_DATA
      && !video.seeking
      && video.buffered.length === 0);
  const mutate = (change: () => void, cameraChanged = false) => {
    pushHistory();
    change();
    if (cameraChanged) invalidatePreviewCameraTrack();
    markDirty();
    syncAll();
  };
  syncEditorBackgroundGallery = mountBackgroundGallery(editorBackgroundGallery, background, (value) => {
    mutate(() => {
      session.settings.background = value;
      session.settings.customBackgroundPath = null;
    });
  });

  splitClip.addEventListener("click", cutAtPlayhead);
  deleteClip.addEventListener("click", deleteSelectedSegment);
  // The playhead input covers the lanes to catch drags, so the menu works off
  // the pointer position rather than whatever element the event landed on.
  timelineGrid.addEventListener("contextmenu", (event) => {
    if (!sessionReady) return;
    event.preventDefault();
    const overZoomLane = Boolean(
      (event.target as HTMLElement).closest("#editor-marker-lane, .recordly-zoom-region"),
    );
    openClipMenu(event, overZoomLane);
  });
  clipMenu.addEventListener("click", (event) => {
    const item = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-clip-action]");
    if (!item || item.disabled) return;
    closeClipMenu();
    switch (item.dataset.clipAction) {
      case "zoom":
      case "zoom-hidden":
        // Place it where the menu was opened rather than at the playhead.
        playhead.value = String(Math.round(menuTimeMs));
        updatePlayhead();
        seekPreview(Math.round(menuTimeMs));
        createMarker(item.dataset.clipAction === "zoom-hidden");
        break;
      case "split":
        // Cut where the menu was opened, not where the playhead happens to be.
        playhead.value = String(Math.round(menuTimeMs));
        updatePlayhead();
        seekPreview(Math.round(menuTimeMs));
        cutAtPlayhead();
        break;
      case "copy":
        copySelectedSegment();
        break;
      case "paste":
        pasteSegment();
        break;
      case "delete":
        deleteSelectedSegment();
        break;
    }
  });
  window.addEventListener("pointerdown", (event) => {
    if (!clipMenu.hidden && !clipMenu.contains(event.target as Node)) closeClipMenu();
  });
  window.addEventListener("blur", closeClipMenu);
  timelineGrid.addEventListener("scroll", closeClipMenu, true);
  document.addEventListener("keydown", (event) => {
    if (editorDisposed) return;
    const target = event.target as HTMLElement | null;
    // Leave typing alone.
    if (target?.closest("input, textarea, select, [contenteditable]")) return;
    const control = event.ctrlKey || event.metaKey;
    if (control && event.key.toLowerCase() === "z" && !event.shiftKey) {
      event.preventDefault();
      undo.click();
      return;
    }
    if (control && (event.key.toLowerCase() === "y" || (event.shiftKey && event.key.toLowerCase() === "z"))) {
      event.preventDefault();
      redo.click();
      return;
    }
    if (control && event.key.toLowerCase() === "c") {
      if (selectedSegment === null) return;
      event.preventDefault();
      copySelectedSegment();
      return;
    }
    if (control && event.key.toLowerCase() === "v") {
      if (!canPaste()) return;
      event.preventDefault();
      pasteSegment();
      return;
    }
    if (event.key === "Escape") {
      closeClipMenu();
      return;
    }
    if (control) return;
    if (event.key.toLowerCase() === "s") {
      event.preventDefault();
      cutAtPlayhead();
      return;
    }
    if (event.key === "Delete" || event.key === "Backspace") {
      if (selectedSegment === null) return;
      event.preventDefault();
      deleteSelectedSegment();
    }
  });
  undo.addEventListener("click", () => {
    const previous = history.pop();
    if (!previous) return;
    future.push(snapshot());
    applySnapshot(previous);
  });
  redo.addEventListener("click", () => {
    const next = future.pop();
    if (!next) return;
    history.push(snapshot());
    applySnapshot(next);
  });
  const createMarker = (hideCursor = false) => {
    const marker: ZoomMarker = {
      timeMs: Number(playhead.value),
      x: 0.5,
      y: 0.5,
      targetScale: session.settings.zoomScale,
      heldUntilMs: null,
      hideCursor,
    };
    pushHistory();
    session.markers.push(marker);
    session.markers.sort((left, right) => left.timeMs - right.timeMs);
    invalidatePreviewCameraTrack();
    selectedMarker = session.markers.indexOf(marker);
    markDirty();
    activateTool("zoom");
    syncAll();
  };
  addMarker.addEventListener("click", () => createMarker(false));
  addOrangeMarker.addEventListener("click", () => createMarker(true));
  addMarkerToolbar.addEventListener("click", () => createMarker(false));
  deleteMarker.addEventListener("click", () => {
    if (selectedMarker === null) return;
    mutate(() => {
      session.markers.splice(selectedMarker!, 1);
      selectedMarker = null;
    }, true);
  });

  for (const input of [markerDepth, markerX, markerY]) {
    input.addEventListener("pointerdown", pushHistory, { passive: true });
  }
  markerDepth.addEventListener("input", () => {
    const marker = selected();
    if (!marker) return;
    marker.targetScale = Number(markerDepth.value) / 100;
    invalidatePreviewCameraTrack();
    markDirty();
    syncMarkerPanel();
    syncTimeline();
    updateLivePreview();
  });
  markerX.addEventListener("input", () => {
    const marker = selected();
    if (!marker) return;
    marker.x = Number(markerX.value) / 100;
    invalidatePreviewCameraTrack();
    markDirty();
    syncMarkerPanel();
    updateLivePreview();
  });
  markerY.addEventListener("input", () => {
    const marker = selected();
    if (!marker) return;
    marker.y = Number(markerY.value) / 100;
    invalidatePreviewCameraTrack();
    markDirty();
    syncMarkerPanel();
    updateLivePreview();
  });
  markerDurationInput.addEventListener("focus", pushHistory, { once: false });
  markerDurationInput.addEventListener("change", () => {
    const marker = selected();
    if (!marker) return;
    setMarkerDuration(marker, Number(markerDurationInput.value) * 1000);
    invalidatePreviewCameraTrack();
    markDirty();
    syncAll();
  });
  markerHideCursor.addEventListener("change", () => mutate(() => {
    const marker = selected();
    if (marker) marker.hideCursor = markerHideCursor.checked;
  }, true));

  const bindRange = (input: HTMLInputElement, apply: (value: number) => void) => {
    input.addEventListener("pointerdown", pushHistory, { passive: true });
    input.addEventListener("input", () => {
      apply(Number(input.value));
      markDirty();
      syncSettings();
    });
  };
  background.addEventListener("change", () => mutate(() => {
    session.settings.background = background.value as RecordingSettings["background"];
    session.settings.customBackgroundPath = null;
  }));
  audioDetached.addEventListener("change", () => mutate(() => {
    // Detaching copies the picture's cuts so the sound starts where it is now
    // and diverges only where it is cut afterwards. Reattaching drops them and
    // the sound follows the picture again.
    session.audioSegments = audioDetached.checked ? clone(session.segments) : [];
  }));
  backgroundEnabled.addEventListener("change", () => mutate(() => {
    session.settings.backgroundEnabled = backgroundEnabled.checked;
  }));
  cursorVisible.addEventListener("change", () => mutate(() => { session.settings.cursorVisible = cursorVisible.checked; }));
  clickEffects.addEventListener("change", () => mutate(() => { session.settings.clickEffectsEnabled = clickEffects.checked; }));
  editorSystemAudioEnabled.addEventListener("change", () => mutate(() => {
    session.settings.audio.systemEnabled = editorSystemAudioEnabled.checked;
  }));
  editorMicrophoneEnabled.addEventListener("change", () => mutate(() => {
    session.settings.audio.microphoneEnabled = editorMicrophoneEnabled.checked;
  }));
  bindRange(editorSystemAudioVolume, (value) => {
    session.settings.audio.systemVolume = value / 100;
  });
  bindRange(editorMicrophoneVolume, (value) => {
    session.settings.audio.microphoneVolume = value / 100;
  });
  bindRange(cursorSize, (value) => { session.settings.cursorSize = value; });
  bindRange(cursorSmoothing, (value) => { session.settings.cursorSmoothing = value; });
  webcamEnabled.addEventListener("change", () => mutate(() => { session.settings.webcam.enabled = webcamEnabled.checked && session.hasWebcam; }));
  webcamMirror.addEventListener("change", () => mutate(() => { session.settings.webcam.mirror = webcamMirror.checked; }));
  webcamReact.addEventListener("change", () => mutate(() => { session.settings.webcam.reactToZoom = webcamReact.checked; }));
  bindRange(webcamSize, (value) => { session.settings.webcam.sizePercent = value; });
  bindRange(webcamRoundness, (value) => { session.settings.webcam.roundness = value; });
  bindRange(webcamShadow, (value) => { session.settings.webcam.shadow = value / 100; });
  customBackground.addEventListener("click", async () => {
    const asset = await invoke<AppearanceAsset | null>("choose_appearance_asset", { kind: "background" });
    if (!asset) return;
    const url = await invoke<string>("publish_appearance_asset", { kind: "background", path: asset.path });
    publishedAssets.set(asset.path, url);
    mutate(() => {
      session.settings.customBackgroundPath = asset.path;
      session.customBackgroundUrl = url;
    });
  });
  customCursor.addEventListener("click", async () => {
    const asset = await invoke<AppearanceAsset | null>("choose_appearance_asset", { kind: "cursor" });
    if (!asset) return;
    const url = await invoke<string>("publish_appearance_asset", { kind: "cursor", path: asset.path });
    publishedAssets.set(asset.path, url);
    mutate(() => {
      session.settings.customCursorPath = asset.path;
      session.customCursorUrl = url;
    });
  });

  // The webview empties the video's surface for the duration of a seek, and the
  // camera frame's own near-black background shows through. Dragging the
  // playhead seeks continuously, so that reads as a strobe. Holding the last
  // decoded frame on a canvas over the video covers the gap: the preview shows
  // the most recent frame instead of nothing, and updates as new ones arrive.
  const scrubContext = scrubFrame.getContext("2d");
  const captureFrame = () => {
    if (!scrubContext || !video.videoWidth) return false;
    if (video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA) return false;
    if (scrubFrame.width !== video.videoWidth || scrubFrame.height !== video.videoHeight) {
      scrubFrame.width = video.videoWidth;
      scrubFrame.height = video.videoHeight;
    }
    try {
      scrubContext.drawImage(video, 0, 0, scrubFrame.width, scrubFrame.height);
    } catch {
      return false;
    }
    return true;
  };
  const holdLastFrame = () => {
    if (captureFrame()) scrubFrame.hidden = false;
  };
  const releaseLastFrame = () => {
    scrubFrame.hidden = true;
  };
  // Each completed seek is a fresh frame to hold, so the held image tracks the
  // drag rather than freezing on wherever it started.
  video.addEventListener("seeked", () => {
    if (scrubbing) captureFrame();
    else releaseLastFrame();
  });

  // A range input fires `input` for every pixel of travel, and assigning
  // currentTime each time restarts the seek before the previous one finished.
  // Coalesce to one seek per frame so the decoder is not asked for positions
  // that are already obsolete.
  let scrubbing = false;
  let queuedSeekMs: number | null = null;
  let seekFrame = 0;
  const flushSeek = () => {
    seekFrame = 0;
    if (queuedSeekMs === null) return;
    const seconds = queuedSeekMs / 1000;
    queuedSeekMs = null;
    video.currentTime = seconds;
    syncSidecarTime(true);
  };
  const seekPreview = (ms: number) => {
    queuedSeekMs = ms;
    if (!seekFrame) seekFrame = requestAnimationFrame(flushSeek);
  };
  playhead.addEventListener("input", () => {
    if (!sessionReady) return;
    const target = clampToTrim(Number(playhead.value));
    if (String(target) !== playhead.value) playhead.value = String(target);
    updatePlayhead();
    updateLivePreview(target);
    seekPreview(target);
  });
  const endScrub = () => {
    if (!scrubbing) return;
    scrubbing = false;
    if (sessionReady) seekPreview(clampToTrim(Number(playhead.value)));
    // The held frame comes down once the video is painting again. Releasing the
    // pointer without moving it leaves nothing to seek to and no seeked event,
    // so a deadline takes it down in that case rather than leaving a still
    // image over a live video.
    let timer = 0;
    const release = () => {
      window.clearTimeout(timer);
      video.removeEventListener("seeked", release);
      releaseLastFrame();
    };
    timer = window.setTimeout(release, 500);
    video.addEventListener("seeked", release);
  };
  playhead.addEventListener("pointerdown", () => {
    scrubbing = true;
    holdLastFrame();
    selectMarker(null);
  });
  playhead.addEventListener("pointerup", endScrub);
  playhead.addEventListener("pointercancel", endScrub);
  playhead.addEventListener("change", endScrub);
  timelineGrid.addEventListener("pointerdown", (event) => {
    const target = event.target as HTMLElement;
    if (!target.closest(".recordly-zoom-region")) selectMarker(null);
    if (!sessionReady || event.button !== 0) return;
    // Reaching a moment used to mean hitting the thin strip above the tracks.
    // Anywhere along the lanes moves the playhead now, except on the pieces
    // that carry their own drag: zoom regions and the trim handles.
    if (target.closest(".recordly-zoom-region, .trim-handle")) return;
    const rect = markerLane.getBoundingClientRect();
    if (rect.width <= 0) return;
    const seekTo = (clientX: number) => {
      const ratio = (clientX - rect.left) / rect.width;
      const at = clampToTrim(
        Math.round(Math.max(0, Math.min(1, ratio)) * session.durationMs),
      );
      playhead.value = String(at);
      updatePlayhead();
      updateLivePreview(at);
      seekPreview(at);
    };
    seekTo(event.clientX);
    const move = (next: PointerEvent) => seekTo(next.clientX);
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop, { once: true });
  });
  video.addEventListener("timeupdate", () => {
    if (!video.paused || playhead.matches(":active")) return;
    playhead.value = String(Math.round(video.currentTime * 1000));
    updatePlayhead();
    updateLivePreview();
    syncSidecarTime(true);
  });
  const playIcon = '<i class="ph-fill ph-play" aria-hidden="true"></i>';
  const pauseIcon = '<i class="ph-fill ph-pause" aria-hidden="true"></i>';
  play.addEventListener("click", async () => {
    if (!video.paused) {
      video.pause();
      return;
    }
    const nowMs = video.currentTime * 1000;
    if (nowMs < session.trimStartMs || nowMs >= session.trimEndMs) {
      video.currentTime = session.trimStartMs / 1000;
      syncSidecarTime(true);
    }
    try {
      // Only a reload is worth blocking the button for; clicking again during
      // one would fight it. play() itself can take a while on a source with
      // sparse keyframes, and the button has to stay usable so a second click
      // can still pause.
      if (previewNeedsRefresh || previewMediaIsStale()) {
        play.disabled = true;
        try {
          await refreshIdlePreview();
        } finally {
          play.disabled = false;
        }
      }
      await video.play();
    } catch (error) {
      previewNeedsRefresh = true;
      videoStatus.textContent = `Preview unavailable: ${String(error)}`;
    }
  });
  video.addEventListener("play", () => {
    play.classList.add("playing");
    play.innerHTML = pauseIcon;
    syncSidecarTime(true);
    if (audioPreview.src) void audioPreview.play().catch(() => {});
    if (webcamPreview.src && !webcamPreview.hidden) void webcamPreview.play().catch(() => {});
    runLiveFrames();
  });
  video.addEventListener("playing", () => {
    previewNeedsRefresh = false;
    videoStatus.textContent = "Preview";
  });
  video.addEventListener("pause", () => {
    play.classList.remove("playing");
    play.innerHTML = playIcon;
    audioPreview.pause();
    webcamPreview.pause();
    stopLiveFrames();
    updateLivePreview();
  });
  video.addEventListener("seeking", () => {
    syncSidecarTime(true);
    updateLivePreview();
  });
  video.addEventListener("stalled", () => {
    videoStatus.textContent = "Preview stalled";
  });
  video.addEventListener("waiting", () => {
    videoStatus.textContent = "Buffering preview…";
  });
  video.addEventListener("canplay", () => {
    if (!previewRefresh) videoStatus.textContent = "Preview";
  });
  skipBack.addEventListener("click", () => {
    video.currentTime = Math.max(session.trimStartMs / 1000, video.currentTime - 5);
  });
  skipForward.addEventListener("click", () => {
    video.currentTime = Math.min(session.trimEndMs / 1000, video.currentTime + 5);
  });
  volume.addEventListener("click", () => {
    video.muted = !video.muted;
    audioPreview.muted = video.muted;
    syncPreviewVolume();
  });
  previewVolume.addEventListener("input", () => {
    previewVolumeLevel = Number(previewVolume.value) / 100;
    syncPreviewVolume();
  });
  const handleEditorVisibility = () => {
    if (document.hidden) {
      hiddenAt = Date.now();
      video.pause();
      audioPreview.pause();
      webcamPreview.pause();
      stopLiveFrames();
      return;
    }
    // How long the editor stayed hidden says nothing about whether the media
    // survived it, so ask the element instead of the clock.
    if (hiddenAt !== null && previewMediaIsStale()) {
      previewNeedsRefresh = true;
      videoStatus.textContent = "Preview ready to refresh";
    }
    hiddenAt = null;
  };
  const cleanupEditor = () => {
    if (editorDisposed) return;
    editorDisposed = true;
    stopLiveFrames();
    previewResizeObserver.disconnect();
    timelineResizeObserver.disconnect();
    stopProcessingProgress?.();
    stopProcessingProgress = null;
    document.removeEventListener("visibilitychange", handleEditorVisibility);
    video.pause();
    audioPreview.pause();
    webcamPreview.pause();
    clickPulsePool.forEach((pulse) => pulse.remove());
    clickPulsePool.length = 0;
  };
  document.addEventListener("visibilitychange", handleEditorVisibility);
  window.addEventListener("pagehide", cleanupEditor, { once: true });
  window.addEventListener("beforeunload", cleanupEditor, { once: true });
  previewStage.addEventListener("click", (event) => {
    const marker = selected();
    if (!marker || !(event.target instanceof HTMLElement) || event.target.closest("button")) return;
    const rect = cameraFrame.getBoundingClientRect();
    if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) return;
    const frameX = (event.clientX - rect.left) / rect.width;
    const frameY = (event.clientY - rect.top) / rect.height;
    mutate(() => {
      marker.x = Math.max(0, Math.min(1, frameX));
      marker.y = Math.max(0, Math.min(1, frameY));
    }, true);
  });
  // Declared, not assigned, so syncTimeline can reach it while building the
  // regions that carry the handles.
  function bindTrimHandle(handle: HTMLElement, edge: "start" | "end") {
    handle.addEventListener("pointerdown", (event) => {
      event.preventDefault();
      event.stopPropagation();
      pushHistory();
      const rect = clipLane.getBoundingClientRect();
      const move = (nextEvent: PointerEvent) => {
        const at = Math.max(
          0,
          Math.min(
            session.durationMs,
            ((nextEvent.clientX - rect.left) / rect.width) * session.durationMs,
          ),
        );
        // The outer edges of the cut list are the recording's in and out
        // points; the pieces between them keep their own bounds.
        const first = session.segments[0];
        const last = session.segments[session.segments.length - 1];
        if (edge === "start") first.startMs = Math.round(Math.min(at, first.endMs - 100));
        else last.endMs = Math.round(Math.max(at, last.startMs + 100));
        syncTrimToSegments();
        markDirty();
        syncTimeline();
      };
      const end = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", end);
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", end, { once: true });
    });
  }

  exportButton.addEventListener("click", (event) => {
    event.stopPropagation();
    exportMenu.hidden = !exportMenu.hidden;
  });
  document.addEventListener("click", (event) => {
    if (!exportMenu.contains(event.target as Node) && event.target !== exportButton) exportMenu.hidden = true;
  });
  save.addEventListener("click", async () => {
    exportMenu.hidden = true;
    setBusy(true);
    progress.value = 0;
    renderPercent.value = "0%";
    renderStatus.textContent = "Choose where to save";
    try {
      const result = await invoke<{ saved: boolean; path: string | null }>("save_editor_session", {
        sessionId,
        changes: {
          markers: session.markers,
          trimStartMs: session.trimStartMs,
          trimEndMs: session.trimEndMs,
          segments: session.segments,
          audioSegments: session.audioSegments,
          settings: session.settings,
        } satisfies EditorChanges,
      });
      if (result.saved && result.path) {
        progress.value = 100;
        renderPercent.value = "100%";
        renderStatus.textContent = "Export complete";
        saveState.textContent = `Exported to ${result.path}`;
      } else {
        renderStatus.textContent = "Export canceled";
        saveState.textContent = "Export canceled";
      }
    } catch (error) {
      const detail = String(error);
      renderStatus.textContent = "Render failed";
      saveState.textContent = `Export failed: ${detail}`;
    } finally {
      setBusy(false);
    }
  });
  discard.addEventListener("click", async () => {
    setBusy(true);
    try {
      await invoke("discard_editor_session", { sessionId });
      await invoke("return_to_hud");
    } catch (error) {
      saveState.textContent = `Discard failed: ${String(error)}`;
      setBusy(false);
    }
  });
  stopProcessingProgress = await listen<ProcessingProgress>("processing-progress", (event) => {
    progress.value = event.payload.percent;
    renderPercent.value = `${event.payload.percent}%`;
    renderStatus.textContent = event.payload.stage;
  });
  if (editorDisposed) {
    stopProcessingProgress();
    stopProcessingProgress = null;
  }
  video.addEventListener("loadeddata", () => { videoStatus.textContent = "Preview"; });
  video.addEventListener("error", () => {
    previewNeedsRefresh = true;
    videoStatus.textContent = "Preview unavailable";
  });

  try {
    session = await invoke<EditorSession>("load_editor_session", { sessionId });
    const webcamTool = toolButtons.find((button) => button.dataset.editorTool === "webcam");
    if (!session.hasWebcam) {
      webcamTool?.setAttribute("hidden", "");
      webcamAvailability.textContent = "No webcam track was captured";
    }
    rulerLabels.replaceChildren(...[0, .25, .5, .75, 1].map((ratio) => {
      const label = document.createElement("span");
      label.textContent = formatElapsed(session.durationMs * ratio);
      return label;
    }));
    syncSession(session);
    videoStatus.textContent = "Preview";
  } catch (error) {
    saveState.textContent = `Could not load session: ${String(error)}`;
  }
}

window.addEventListener("DOMContentLoaded", () => {
  document.body.dataset.surface = editorWindow ? "editor" : settingsWindow ? "settings" : "hud";
  // A transparent GTK/Tauri HUD can regain a click-through input region after
  // the webview finishes loading or the compositor restores the window.  The
  // native side also restores it, but repeat the operation from the actual
  // HUD document so source selection never depends on a timing race.
  if (tauriAvailable && currentWindowLabel === "main") {
    void getCurrentWindow().setIgnoreCursorEvents(false).catch(() => {});
  }
  if (editorWindow) {
    void setupEditor();
    return;
  }
  if (tauriAvailable && currentWindowLabel === "main" && requestedEditorSession) {
    void invoke("open_editor_window", { sessionId: requestedEditorSession }).then(() => {
      window.location.replace("index.html");
    });
    return;
  }
  const selectMonitor = document.querySelector<HTMLButtonElement>("#hud-select-source");
  const changeMonitor = document.querySelector<HTMLButtonElement>("#change-monitor");
  const startRecording = document.querySelector<HTMLButtonElement>("#start-recording");
  const stopRecording = document.querySelector<HTMLButtonElement>("#stop-recording");
  const sourceEmpty = document.querySelector<HTMLElement>("#source-empty");
  const recordingLive = document.querySelector<HTMLElement>("#recording-live");
  const renderingLive = document.querySelector<HTMLElement>("#rendering-live");
  const sessionStateLabel = document.querySelector<HTMLElement>("#session-state-label");
  const recordingElapsed = document.querySelector<HTMLElement>("#recording-elapsed");
  const fps = document.querySelector<HTMLSelectElement>("#recording-fps");
  const quality = document.querySelector<HTMLSelectElement>("#recording-quality");
  const background = document.querySelector<HTMLSelectElement>("#recording-background");
  const cursorVisible = document.querySelector<HTMLButtonElement>("#cursor-visible");
  const cursorSmoothing = document.querySelector<HTMLInputElement>("#cursor-smoothing");
  const cursorSmoothingOutput = document.querySelector<HTMLOutputElement>("#cursor-smoothing-output");
  const cursorSize = document.querySelector<HTMLInputElement>("#cursor-size");
  const cursorSizeOutput = document.querySelector<HTMLOutputElement>("#cursor-size-output");
  const zoomScale = document.querySelector<HTMLInputElement>("#zoom-scale");
  const zoomScaleOutput = document.querySelector<HTMLOutputElement>("#zoom-scale-output");
  const zoomDemo = document.querySelector<HTMLElement>("#zoom-demo");
  const zoomDemoCursor = document.querySelector<HTMLImageElement>("#zoom-demo-cursor");
  const clickEffectsEnabled = document.querySelector<HTMLButtonElement>("#click-effects-enabled");
  const chooseCustomCursor = document.querySelector<HTMLButtonElement>("#choose-custom-cursor");
  const clearCustomCursor = document.querySelector<HTMLButtonElement>("#clear-custom-cursor");
  const customCursorNote = document.querySelector<HTMLElement>("#custom-cursor-note");
  const chooseCustomBackground = document.querySelector<HTMLButtonElement>("#choose-custom-background");
  const clearCustomBackground = document.querySelector<HTMLButtonElement>("#clear-custom-background");
  const customBackgroundNote = document.querySelector<HTMLElement>("#custom-background-note");
  const systemAudioEnabled = document.querySelector<HTMLButtonElement>("#system-audio-enabled");
  const microphoneEnabled = document.querySelector<HTMLButtonElement>("#microphone-enabled");
  const systemAudioVolume = document.querySelector<HTMLInputElement>("#system-audio-volume");
  const microphoneVolume = document.querySelector<HTMLInputElement>("#microphone-volume");
  const systemAudioVolumeOutput = document.querySelector<HTMLOutputElement>("#system-audio-volume-output");
  const microphoneVolumeOutput = document.querySelector<HTMLOutputElement>("#microphone-volume-output");
  const systemAudioDevice = document.querySelector<HTMLSelectElement>("#system-audio-device");
  const microphoneDevice = document.querySelector<HTMLSelectElement>("#microphone-device");
  const webcamEnabled = document.querySelector<HTMLButtonElement>("#webcam-enabled");
  const webcamMirror = document.querySelector<HTMLInputElement>("#webcam-mirror");
  const webcamDevice = document.querySelector<HTMLSelectElement>("#webcam-device");
  const webcamNote = document.querySelector<HTMLElement>("#webcam-note");
  const hideBottomBar = document.querySelector<HTMLInputElement>("#hide-bottom-bar");
  const barHeightCorrection = document.querySelector<HTMLInputElement>("#bar-height-correction");
  const barHeightCorrectionOutput = document.querySelector<HTMLOutputElement>("#bar-height-correction-output");
  const windowCropNote = document.querySelector<HTMLElement>("#window-crop-note");
  const sourceSelected = document.querySelector<HTMLElement>("#source-selected");
  const selectedSourceMeta = document.querySelector<HTMLElement>("#selected-source-meta");
  const status = document.querySelector<HTMLElement>("#capture-status");
  const settingsPopover = document.querySelector<HTMLElement>("#settings-popover");
  const toggleSettings = document.querySelector<HTMLButtonElement>("#toggle-settings");
  const hudSystemAudio = document.querySelector<HTMLButtonElement>("#hud-system-audio");
  const hudMicrophone = document.querySelector<HTMLButtonElement>("#hud-microphone");
  const hudLiveState = document.querySelector<HTMLElement>("#hud-live-state");
  const hudSessionCopy = document.querySelector<HTMLElement>("#hud-session-copy");
  const hudDragHandle = document.querySelector<HTMLElement>(".hud-drag-handle");
  const hudRail = document.querySelector<HTMLElement>(".action-rail");
  const settingsTabs = [...document.querySelectorAll<HTMLButtonElement>("[data-settings-tab]")];
  const settingsPanels = [...document.querySelectorAll<HTMLElement>("[data-settings-panel]")];
  const recordingBackgroundGallery = document.querySelector<HTMLElement>('[data-background-gallery="recording"]');
  let systemAudioOn = true;
  let microphoneOn = true;
  let cursorOn = true;
  let clickEffectsOn = true;
  let customCursorPath: string | null = null;
  let customBackgroundPath: string | null = null;
  let customCursorLabel: string | null = null;
  let customBackgroundLabel: string | null = null;
  let webcamOn = false;
  let webcamAvailable = false;
  let configuredWebcamDeviceId: string | null = null;
  let previewActive = false;
  let selectedSourceKind: CaptureSourceKind = "monitor";
  let recordingStartedAt: number | null = null;
  let recordingTimer: number | null = null;
  let settingsReady = false;
  let settingsOpen = settingsWindow;
  let syncRecordingBackgroundGallery = () => {};

  hudDragHandle?.addEventListener("pointerdown", (event) => {
    if (event.button !== 0 || !tauriAvailable) return;
    event.preventDefault();
    void getCurrentWindow().startDragging();
  });
  hudRail?.addEventListener("animationend", (event) => {
    if (event.animationName === "pikscreen-hud-shell") hudRail.classList.remove("hud-intro");
  });

  async function setWindowSurface(surface: "hud" | "settings") {
    if (settingsWindow || surface === "settings") return;
    try {
      await invoke("set_window_surface", { surface });
    } catch (error) {
      if (status) status.textContent = `Could not update the PikScreen window: ${String(error)}`;
    }
  }
  function syncSettingsPopover() {
    if (settingsWindow) {
      if (settingsPopover) settingsPopover.hidden = false;
      return;
    }
    if (settingsPopover) settingsPopover.hidden = true;
    if (toggleSettings) toggleSettings.setAttribute("aria-expanded", "false");
    void setWindowSurface("hud");
  }

  const describeCrop = (crop: Crop, sourceKind: CaptureSourceKind) => sourceKind === "window"
    ? `${crop.width} × ${crop.height} · selected window`
    : crop.excludedBottom > 0
    ? `${crop.width} × ${crop.height} · bottom ${crop.excludedBottom}px hidden`
    : `${crop.width} × ${crop.height} · full monitor`;
  const previewSettings = (): PreviewSettings => ({
    hideBottomBar: selectedSourceKind === "monitor" && (hideBottomBar?.checked ?? true),
    barHeightCorrection: Number(barHeightCorrection?.value ?? 0),
  });
  const recordingSettings = (): RecordingSettings => ({
    fps: Number(fps?.value ?? 120),
    quality: (quality?.value ?? "high") as RecordingSettings["quality"],
    background: (background?.value ?? "tahoeDark") as RecordingSettings["background"],
    backgroundEnabled: true,
    customBackgroundPath,
    cursorVisible: cursorOn,
    customCursorPath,
    cursorSmoothing: Number(cursorSmoothing?.value ?? 35),
    cursorSize: Number(cursorSize?.value ?? 100),
    zoomScale: Number(zoomScale?.value ?? 180) / 100,
    clickEffectsEnabled: clickEffectsOn,
    audio: {
      systemEnabled: systemAudioOn,
      microphoneEnabled: microphoneOn,
      systemVolume: Number(systemAudioVolume?.value ?? 100) / 100,
      microphoneVolume: Number(microphoneVolume?.value ?? 100) / 100,
      systemDevice: systemAudioDevice?.value || null,
      microphoneDevice: microphoneDevice?.value || null,
    },
    webcam: {
      enabled: webcamOn && webcamAvailable,
      deviceId: (configuredWebcamDeviceId ?? webcamDevice?.value) || null,
      mirror: webcamMirror?.checked ?? true,
      sizePercent: 40,
      margin: 24,
      roundness: 90,
      shadow: 0.67,
      reactToZoom: true,
    },
  });
  const savedSettings = (): SavedSettings => ({
    ...recordingSettings(),
    hideBottomBar: hideBottomBar?.checked ?? true,
    barHeightCorrection: Number(barHeightCorrection?.value ?? 0),
  });
  async function persistSettings() {
    if (!settingsReady) return;
    try {
      await invoke("save_settings", { settings: savedSettings() });
    } catch (error) {
      if (status) status.textContent = `Could not save recording settings: ${String(error)}`;
    }
  }
  function setSessionPhase(next: SessionPhase) {
    previewActive = next === "preview";
    document.body.dataset.sessionState = next;
    if (sessionStateLabel) {
      sessionStateLabel.textContent = {
        ready: "Ready to record",
        preview: "Source selected",
        recording: "Recording",
        rendering: "Finalizing recording",
      }[next];
    }
    if (hudSessionCopy) {
      hudSessionCopy.textContent = {
        ready: "Choose a screen or window",
        preview: "Source selected · ready to record",
        recording: "Recording in progress",
        rendering: "Preserving source tracks",
      }[next];
    }
    if (sourceEmpty) sourceEmpty.hidden = next !== "ready";
    if (sourceSelected) sourceSelected.hidden = next !== "preview";
    if (recordingLive) recordingLive.hidden = next !== "recording";
    if (renderingLive) renderingLive.hidden = next !== "rendering";
    if (hudLiveState) hudLiveState.hidden = next !== "recording";
    if (selectMonitor) {
      selectMonitor.hidden = next !== "ready";
      if (next === "ready") {
        selectMonitor.disabled = false;
        const label = selectMonitor.querySelector<HTMLElement>("[data-hud-source-label]");
        if (label) label.textContent = "Screen";
      }
    }
    if (changeMonitor) changeMonitor.hidden = next !== "preview";
    if (startRecording) {
      startRecording.hidden = next === "recording" || next === "rendering";
      startRecording.disabled = next !== "preview";
    }
    if (stopRecording) {
      stopRecording.hidden = next !== "recording";
      stopRecording.disabled = next !== "recording";
    }
    if (next === "recording") {
      settingsOpen = false;
      syncSettingsPopover();
    } else if (next === "rendering") {
      settingsOpen = false;
      if (settingsPopover) settingsPopover.hidden = true;
      if (toggleSettings) toggleSettings.setAttribute("aria-expanded", "false");
      void setWindowSurface("hud");
    } else if (!settingsOpen) {
      void setWindowSurface("hud");
    }
  }
  function setPreviewActive(active: boolean) {
    setSessionPhase(active ? "preview" : "ready");
  }
  function syncSourceCropControls() {
    const windowSource = selectedSourceKind === "window";
    if (hideBottomBar) hideBottomBar.disabled = windowSource;
    if (barHeightCorrection) barHeightCorrection.disabled = windowSource;
    if (windowCropNote) windowCropNote.hidden = !windowSource;
  }

  toggleSettings?.addEventListener("click", () => {
    if (settingsWindow || document.body.dataset.sessionState === "recording" || document.body.dataset.sessionState === "rendering") return;
    void invoke("open_settings_window").catch(error => {
      if (status) status.textContent = `Could not open settings: ${String(error)}`;
    });
  });
  function syncBinaryControl(toggle: HTMLButtonElement | null, enabled: boolean, onLabel: string, offLabel: string) {
    if (!toggle) return;
    toggle.setAttribute("aria-pressed", String(enabled));
    const label = toggle.querySelector("span");
    const state = toggle.querySelector("strong");
    if (label) label.textContent = enabled ? onLabel : offLabel;
    if (state) state.textContent = enabled ? "On" : "Off";
  }
  function syncAudioControl(toggle: HTMLButtonElement | null, slider: HTMLInputElement | null, output: HTMLOutputElement | null, enabled: boolean) {
    if (toggle) {
      toggle.setAttribute("aria-pressed", String(enabled));
      const state = toggle.querySelector("strong");
      if (state) state.textContent = enabled ? "On" : "Off";
    }
    if (slider) {
      slider.disabled = !enabled;
      syncRangeFill(slider);
    }
    if (output && slider) output.value = `${slider.value}%`;
  }
  function syncHudAudioToggle(toggle: HTMLButtonElement | null, enabled: boolean, label: string) {
    if (!toggle) return;
    toggle.setAttribute("aria-pressed", String(enabled));
    toggle.title = `${label}: ${enabled ? "on" : "off"}`;
  }
  function syncWebcamControl() {
    if (webcamEnabled) {
      webcamEnabled.disabled = !webcamAvailable;
      webcamEnabled.setAttribute("aria-pressed", String(webcamOn && webcamAvailable));
      const state = webcamEnabled.querySelector("strong");
      if (state) state.textContent = webcamOn && webcamAvailable ? "On" : "Off";
    }
    if (webcamMirror) webcamMirror.disabled = !webcamAvailable || !webcamOn;
    if (webcamDevice) webcamDevice.disabled = !webcamAvailable || !webcamOn;
  }
  function syncSavedSettingsUi() {
    queueMicrotask(syncRangeFills);
    syncAudioControl(systemAudioEnabled, systemAudioVolume, systemAudioVolumeOutput, systemAudioOn);
    syncAudioControl(microphoneEnabled, microphoneVolume, microphoneVolumeOutput, microphoneOn);
    syncHudAudioToggle(hudSystemAudio, systemAudioOn, "System audio");
    syncHudAudioToggle(hudMicrophone, microphoneOn, "Microphone");
    if (cursorSmoothing) {
      syncRangeFill(cursorSmoothing);
      if (cursorSmoothingOutput) cursorSmoothingOutput.value = `${cursorSmoothing.value}%`;
    }
    syncCursorSize();
    syncZoomScale();
    if (barHeightCorrectionOutput && barHeightCorrection) {
      barHeightCorrectionOutput.value = `${barHeightCorrection.value} px`;
    }
    syncWebcamControl();
    syncSourceCropControls();
    syncBinaryControl(cursorVisible, cursorOn, "Show cursor", "Hide cursor");
    syncBinaryControl(clickEffectsEnabled, clickEffectsOn, "Show pulses", "Hide pulses");
    if (customCursorNote) customCursorNote.textContent = customCursorLabel ?? "Tahoe cursor set";
    if (clearCustomCursor) clearCustomCursor.disabled = !customCursorPath;
    if (customBackgroundNote) customBackgroundNote.textContent = customBackgroundLabel ?? "Using selected Tahoe background";
    if (clearCustomBackground) clearCustomBackground.disabled = !customBackgroundPath;
    syncRecordingBackgroundGallery();
  }
  function syncZoomScale() {
    if (!zoomScale) return;
    const zoom = Number(zoomScale.value) / 100;
    syncRangeFill(zoomScale);
    if (zoomScaleOutput) zoomScaleOutput.value = `${zoom.toFixed(2).replace(/0$/, "")}×`;
    if (zoomDemo) zoomDemo.style.setProperty("--zoom-viewport-size", `${100 / zoom}%`);
  }
  function syncCursorSize() {
    const size = Number(cursorSize?.value ?? 100);
    if (cursorSize) {
      syncRangeFill(cursorSize);
      if (cursorSizeOutput) cursorSizeOutput.value = `${size}%`;
    }
    if (zoomDemo) {
      zoomDemo.style.setProperty("--zoom-demo-cursor-size", `${Math.round(size * 0.29)}px`);
    }
    if (zoomDemoCursor) zoomDemoCursor.src = tahoeCursorUrl;
  }
  function applySavedSettings(settings: SavedSettings) {
    if (fps) fps.value = String(settings.fps);
    if (quality) quality.value = settings.quality;
    if (background) background.value = settings.background;
    customBackgroundPath = settings.customBackgroundPath ?? null;
    customBackgroundLabel = customBackgroundPath?.split("/").pop() ?? null;
    cursorOn = settings.cursorVisible;
    customCursorPath = settings.customCursorPath ?? null;
    customCursorLabel = customCursorPath?.split("/").pop() ?? null;
    if (cursorSmoothing) cursorSmoothing.value = String(settings.cursorSmoothing);
    if (cursorSize) cursorSize.value = String(settings.cursorSize);
    if (zoomScale) zoomScale.value = String(Math.round((settings.zoomScale ?? 1.8) * 100));
    clickEffectsOn = settings.clickEffectsEnabled;
    systemAudioOn = settings.audio.systemEnabled;
    microphoneOn = settings.audio.microphoneEnabled;
    if (systemAudioVolume) systemAudioVolume.value = String(Math.round(settings.audio.systemVolume * 100));
    if (microphoneVolume) microphoneVolume.value = String(Math.round(settings.audio.microphoneVolume * 100));
    if (hideBottomBar) hideBottomBar.checked = settings.hideBottomBar;
    if (barHeightCorrection) barHeightCorrection.value = String(settings.barHeightCorrection);
    webcamOn = settings.webcam.enabled;
    configuredWebcamDeviceId = settings.webcam.deviceId;
    if (webcamMirror) webcamMirror.checked = settings.webcam.mirror;
    syncSavedSettingsUi();
  }
  function fillAudioDevices(select: HTMLSelectElement | null, options: AudioDevice[], preferred: string | null, fallback: string | null) {
    if (!select) return;
    select.replaceChildren(...options.map(device => {
      const option = document.createElement("option");
      option.value = device.id;
      option.textContent = device.label;
      return option;
    }));
    const selected = options.some(device => device.id === preferred)
      ? preferred
      : options.some(device => device.id === fallback)
        ? fallback
        : options[0]?.id ?? "";
    select.value = selected ?? "";
  }
  function setSettingsDisabled(disabled: boolean) {
    [fps, quality, background, cursorVisible, cursorSmoothing, cursorSize, zoomScale, clickEffectsEnabled, chooseCustomCursor, clearCustomCursor, chooseCustomBackground, clearCustomBackground, systemAudioEnabled, microphoneEnabled, systemAudioDevice, microphoneDevice, webcamEnabled, webcamMirror, webcamDevice, hideBottomBar, barHeightCorrection].forEach(control => {
      if (control) control.disabled = disabled;
    });
    if (systemAudioVolume) systemAudioVolume.disabled = disabled || !systemAudioOn;
    if (microphoneVolume) microphoneVolume.disabled = disabled || !microphoneOn;
    if (systemAudioDevice) systemAudioDevice.disabled = disabled || !systemAudioOn;
    if (microphoneDevice) microphoneDevice.disabled = disabled || !microphoneOn;
    if (webcamEnabled) webcamEnabled.disabled = disabled || !webcamAvailable;
    if (webcamMirror) webcamMirror.disabled = disabled || !webcamAvailable || !webcamOn;
    if (webcamDevice) webcamDevice.disabled = disabled || !webcamAvailable || !webcamOn;
    if (!disabled) syncSourceCropControls();
  }
  function updateRecordingElapsed() {
    if (recordingStartedAt !== null && recordingElapsed) recordingElapsed.textContent = formatElapsed(performance.now() - recordingStartedAt);
  }
  function startRecordingTimer() {
    stopRecordingTimer();
    recordingStartedAt = performance.now();
    updateRecordingElapsed();
    recordingTimer = window.setInterval(updateRecordingElapsed, 100);
  }
  function stopRecordingTimer() {
    if (recordingTimer !== null) window.clearInterval(recordingTimer);
    recordingTimer = null;
    updateRecordingElapsed();
  }
  async function refreshPreviewCrop() {
    if (!previewActive) return;
    try {
      const result = await invoke<PreviewInfo>("update_preview_crop", { previewSettings: previewSettings() });
      selectedSourceKind = result.sourceKind;
      const sourceLabel = selectMonitor?.querySelector<HTMLElement>("[data-hud-source-label]");
      if (sourceLabel) sourceLabel.textContent = result.sourceKind === "window" ? "Window" : "Screen";
      syncSourceCropControls();
      if (selectedSourceMeta) selectedSourceMeta.textContent = describeCrop(result.crop, result.sourceKind);
      if (status) status.textContent = result.detail;
    } catch (error) {
      if (status) status.textContent = String(error);
    }
  }

  const activateSettingsPanel = (panel: string) => {
    settingsTabs.forEach((button) => {
      const active = button.dataset.settingsTab === panel;
      button.classList.toggle("active", active);
      button.setAttribute("aria-selected", String(active));
    });
    settingsPanels.forEach((section) => {
      section.classList.toggle("active", section.dataset.settingsPanel === panel);
    });
    document.querySelector<HTMLElement>(".settings-content")?.scrollTo({ top: 0 });
  };
  settingsTabs.forEach((button) => {
    button.addEventListener("click", () => activateSettingsPanel(button.dataset.settingsTab ?? "video"));
  });
  const initialSettingsPanel = new URLSearchParams(window.location.search).get("tab");
  if (settingsWindow && initialSettingsPanel && settingsPanels.some((panel) => panel.dataset.settingsPanel === initialSettingsPanel)) {
    activateSettingsPanel(initialSettingsPanel);
  }
  syncRecordingBackgroundGallery = background
    ? mountBackgroundGallery(recordingBackgroundGallery, background, (value) => {
        background.value = value;
        customBackgroundPath = null;
        customBackgroundLabel = null;
        syncSavedSettingsUi();
        void persistSettings();
      })
    : () => {};

  systemAudioEnabled?.addEventListener("click", () => {
    systemAudioOn = !systemAudioOn;
    syncAudioControl(systemAudioEnabled, systemAudioVolume, systemAudioVolumeOutput, systemAudioOn);
    syncHudAudioToggle(hudSystemAudio, systemAudioOn, "System audio");
    void persistSettings();
  });
  hudSystemAudio?.addEventListener("click", () => {
    systemAudioOn = !systemAudioOn;
    syncAudioControl(systemAudioEnabled, systemAudioVolume, systemAudioVolumeOutput, systemAudioOn);
    syncHudAudioToggle(hudSystemAudio, systemAudioOn, "System audio");
    void persistSettings();
  });
  cursorVisible?.addEventListener("click", () => {
    cursorOn = !cursorOn;
    syncBinaryControl(cursorVisible, cursorOn, "Show cursor", "Hide cursor");
    syncCursorSize();
    void persistSettings();
  });
  clickEffectsEnabled?.addEventListener("click", () => {
    clickEffectsOn = !clickEffectsOn;
    syncBinaryControl(clickEffectsEnabled, clickEffectsOn, "Show pulses", "Hide pulses");
    void persistSettings();
  });
  chooseCustomCursor?.addEventListener("click", async () => {
    try {
      const asset = await invoke<AppearanceAsset | null>("choose_appearance_asset", { kind: "cursor" });
      if (!asset) return;
      customCursorPath = asset.path;
      customCursorLabel = asset.label;
      syncSavedSettingsUi();
      await persistSettings();
    } catch (error) {
      if (status) status.textContent = `Could not import custom cursor: ${String(error)}`;
    }
  });
  clearCustomCursor?.addEventListener("click", async () => {
    customCursorPath = null;
    customCursorLabel = null;
    syncSavedSettingsUi();
    await persistSettings();
  });
  chooseCustomBackground?.addEventListener("click", async () => {
    try {
      const asset = await invoke<AppearanceAsset | null>("choose_appearance_asset", { kind: "background" });
      if (!asset) return;
      customBackgroundPath = asset.path;
      customBackgroundLabel = asset.label;
      syncSavedSettingsUi();
      await persistSettings();
    } catch (error) {
      if (status) status.textContent = `Could not import custom background: ${String(error)}`;
    }
  });
  clearCustomBackground?.addEventListener("click", async () => {
    customBackgroundPath = null;
    customBackgroundLabel = null;
    syncSavedSettingsUi();
    await persistSettings();
  });
  microphoneEnabled?.addEventListener("click", () => {
    microphoneOn = !microphoneOn;
    syncAudioControl(microphoneEnabled, microphoneVolume, microphoneVolumeOutput, microphoneOn);
    syncHudAudioToggle(hudMicrophone, microphoneOn, "Microphone");
    void persistSettings();
  });
  hudMicrophone?.addEventListener("click", () => {
    microphoneOn = !microphoneOn;
    syncAudioControl(microphoneEnabled, microphoneVolume, microphoneVolumeOutput, microphoneOn);
    syncHudAudioToggle(hudMicrophone, microphoneOn, "Microphone");
    void persistSettings();
  });
  webcamEnabled?.addEventListener("click", () => {
    webcamOn = !webcamOn;
    syncWebcamControl();
    void persistSettings();
  });
  systemAudioVolume?.addEventListener("input", () => syncAudioControl(systemAudioEnabled, systemAudioVolume, systemAudioVolumeOutput, systemAudioOn));
  microphoneVolume?.addEventListener("input", () => syncAudioControl(microphoneEnabled, microphoneVolume, microphoneVolumeOutput, microphoneOn));
  systemAudioVolume?.addEventListener("change", () => void persistSettings());
  microphoneVolume?.addEventListener("change", () => void persistSettings());
  cursorSmoothing?.addEventListener("input", () => {
    syncRangeFill(cursorSmoothing);
    if (cursorSmoothingOutput) cursorSmoothingOutput.value = `${cursorSmoothing.value}%`;
  });
  cursorSmoothing?.addEventListener("change", () => void persistSettings());
  cursorSize?.addEventListener("input", () => {
    syncCursorSize();
  });
  cursorSize?.addEventListener("change", () => void persistSettings());
  zoomScale?.addEventListener("input", syncZoomScale);
  zoomScale?.addEventListener("change", () => void persistSettings());
  fps?.addEventListener("change", () => void persistSettings());
  quality?.addEventListener("change", () => void persistSettings());
  background?.addEventListener("change", () => {
    void persistSettings();
  });
  systemAudioDevice?.addEventListener("change", () => void persistSettings());
  microphoneDevice?.addEventListener("change", () => void persistSettings());
  webcamMirror?.addEventListener("change", () => void persistSettings());
  webcamDevice?.addEventListener("change", () => {
    configuredWebcamDeviceId = webcamDevice.value || null;
    void persistSettings();
  });
  hideBottomBar?.addEventListener("change", () => {
    void refreshPreviewCrop();
    void persistSettings();
  });
  barHeightCorrection?.addEventListener("input", () => {
    if (barHeightCorrectionOutput) barHeightCorrectionOutput.value = `${barHeightCorrection.value} px`;
    void refreshPreviewCrop();
  });
  barHeightCorrection?.addEventListener("change", () => void persistSettings());
  syncSavedSettingsUi();
  if (settingsWindow) {
    syncSettingsPopover();
    window.scrollTo(0, 0);
    settingsPopover?.scrollTo(0, 0);
  }
  setSessionPhase("ready");
  void (async () => {
    let saved: SavedSettings | null = null;
    try {
      saved = await invoke<SavedSettings>("load_settings");
      applySavedSettings(saved);
    } catch (error) {
      if (status) status.textContent = `Could not load saved settings: ${String(error)}`;
    }
    try {
      const devices = await invoke<AudioDevices>("audio_devices");
      fillAudioDevices(systemAudioDevice, devices.systemOutputs, saved?.audio.systemDevice ?? null, devices.defaultSystemOutput);
      fillAudioDevices(microphoneDevice, devices.microphones, saved?.audio.microphoneDevice ?? null, devices.defaultMicrophone);
    } catch (error) {
      if (status) status.textContent = `Could not list audio devices: ${String(error)}`;
    }
    try {
      const devices = await invoke<WebcamDevice[]>("webcam_devices");
      webcamAvailable = devices.length > 0;
      if (webcamDevice) {
        webcamDevice.replaceChildren(...devices.map(device => {
          const option = document.createElement("option");
          option.value = device.id;
          option.textContent = device.label;
          return option;
        }));
        webcamDevice.value = devices.some(device => device.id === configuredWebcamDeviceId)
          ? configuredWebcamDeviceId ?? ""
          : devices[0]?.id ?? "";
      }
      if (webcamNote) webcamNote.textContent = webcamAvailable
        ? "Recordly-style bubble: 40% bottom-right, mirrored, with a soft shadow."
        : "No V4L2 webcam is connected. Screen recording works normally.";
      syncWebcamControl();
    } catch (error) {
      webcamAvailable = false;
      if (webcamNote) webcamNote.textContent = `Could not list webcam devices: ${String(error)}`;
      syncWebcamControl();
    } finally {
      settingsReady = true;
    }
  })();

  void listen<{ count: number; time_ms: number; x: number; y: number }>("marker-added", ({ payload }) => {
    if (status) status.textContent = `Zoom marker ${payload.count} at ${(payload.x * 100).toFixed(0)}%, ${(payload.y * 100).toFixed(0)}% — ${(payload.time_ms / 1000).toFixed(1)}s.`;
  });
  void listen<string>("marker-error", ({ payload }) => { if (status) status.textContent = payload; });
  void listen<SavedSettings>("settings-saved", ({ payload }) => applySavedSettings(payload));
  void listen<ProcessingProgress>("processing-progress", ({ payload }) => {
    if (status) status.textContent = `${payload.stage} — ${payload.percent}%`;
  });
  void listen("stop-recording-requested", () => {
    if (document.body.dataset.sessionState === "recording" && stopRecording && !stopRecording.disabled) {
      stopRecording.click();
    }
  });
  selectMonitor?.addEventListener("click", async event => {
    if (!status) return;
    status.textContent = "Waiting for the screen or window picker…";
    selectMonitor.disabled = true;
    try {
      const click = event as MouseEvent;
      const result = await invoke<PreviewInfo>("select_monitor", {
        click: { clientX: click.clientX, clientY: click.clientY, screenX: click.screenX, screenY: click.screenY, scaleFactor: window.devicePixelRatio },
        previewSettings: previewSettings(),
      });
      status.textContent = result.detail;
      selectedSourceKind = result.sourceKind;
      syncSourceCropControls();
      if (selectedSourceMeta) selectedSourceMeta.textContent = describeCrop(result.crop, result.sourceKind);
      setPreviewActive(true);
    } catch (error) {
      status.textContent = String(error);
      selectMonitor.disabled = false;
    }
  });
  changeMonitor?.addEventListener("click", async () => {
    changeMonitor.disabled = true;
    try {
      await invoke("cancel_preview");
      setPreviewActive(false);
      if (selectMonitor) selectMonitor.disabled = false;
      selectedSourceKind = "monitor";
      syncSourceCropControls();
      if (status) status.textContent = "Choose a screen or window to start a recording.";
    } catch (error) {
      if (status) status.textContent = String(error);
    } finally { changeMonitor.disabled = false; }
  });
  startRecording?.addEventListener("click", async () => {
    if (!status) return;
    startRecording.disabled = true;
    startRecording.setAttribute("aria-busy", "true");
    status.textContent = "Preparing the selected source…";
    if (hudSessionCopy) hudSessionCopy.textContent = "Starting recording";
    try {
      const result = await invoke<{ detail: string }>("start_recording", { settings: recordingSettings() });
      status.textContent = result.detail;
      setSessionPhase("recording");
      startRecordingTimer();
      setSettingsDisabled(true);
    } catch (error) {
      const detail = String(error);
      status.textContent = detail;
      setSessionPhase("preview");
      if (startRecording) startRecording.disabled = false;
      if (selectedSourceKind === "window") {
        window.alert(`PikScreen could not start the selected window recording:\n\n${detail}`);
      }
    } finally {
      startRecording.removeAttribute("aria-busy");
    }
  });
  stopRecording?.addEventListener("click", async () => {
    if (!status) return;
    stopRecording.disabled = true;
    stopRecordingTimer();
    setSessionPhase("rendering");
    try {
      status.textContent = "Finalizing recording…";
      const completed = await invoke<{ sourcePath: string; sessionId: string }>("stop_recording");
      status.textContent = "Opening PikScreen Editor…";
      setSessionPhase("ready");
      await invoke("open_editor_window", { sessionId: completed.sessionId });
      window.location.replace("index.html");
      return;
    } catch (error) {
      status.textContent = String(error);
      setSessionPhase("recording");
      startRecordingTimer();
      setSettingsDisabled(false);
    }
  });
});
