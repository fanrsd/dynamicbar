import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
import {
  ArrowBigUp, BatteryFull, BatteryLow, Bell, Bluetooth, Camera, ChevronRight, Clipboard, Cloud, CloudDrizzle, CloudFog,
  CloudLightning, CloudRain, CloudSnow, CloudSun, Coffee, Download, EyeOff, FolderDown, Globe, Hand, Hash, House,
  LayoutGrid, Lock, Mic, MicOff, Moon, Music, Pause, Play, Power, RotateCcw, Scissors, Settings, SkipBack, SkipForward,
  Square, Sun, Timer as TimerIcon, Usb, Volume2, VolumeX, Wifi, WifiOff, X, Zap, type LucideIcon,
} from "lucide-react";

type Media = { title: string; artist: string; playing: boolean; art: string | null } | null;
type Status = { battery: number | null; charging: boolean; online: boolean; wifi: boolean };
type Stats = { cpu: number; ram: number; down: number; up: number };
type InUse = { camera: string[]; mic: string[] };
type Drive = { letter: string; name: string };
type Monitor = { id: number; name: string; min: number; max: number; value: number };
type Weather = { temp: number; code: number };
/// `aumid`: the source app's AppUserModelId (null for notifications posted to the local API).
type Notif = { app: string; title: string; body: string; icon: string | null; aumid: string | null };
/// A notification kept in the island's history until dismissed.
type Notice = Notif & { id: number; time: number };
/// Mirrors wapriv::Settings (PascalCase keys, shared with the standalone WAPriv settings file).
type WAPriv = { Enabled: boolean; Spotlight: boolean; Blur: number; HoleW: number; HoleH: number };
/// `total` null = stopwatch. `paused` holds the elapsed ms while paused.
type Timer = { label: string; total: number | null; start: number; paused: number | null };
type Toast =
  /// One-line status toast; `sticky` keeps it visible over an open panel (e.g. eject results);
  /// `level` (0–100) turns it into a HUD bar.
  | { kind: "info"; icon: LucideIcon; label: string; value: string; tone?: string; sticky?: boolean; level?: number }
  | { kind: "clipboard"; text: string }
  | ({ kind: "notification" } & Notif)
  | { kind: "greeting"; name: string }
  | { kind: "usb"; drive: Drive }
  | { kind: "download"; name: string; path: string };
type Tab = "home" | "notifs" | "control" | "timer" | "privacy" | "settings";
const setWAPriv = (settings: WAPriv) => invoke("set_wapriv", { settings });
const openPath = (path: string, reveal: boolean) => invoke("open_path", { path, reveal });
const MAX_NOTICES = 50;
let noticeId = 0;

const DEFAULTS = {
  greeting: true,
  hideCapture: false,
  position: "top" as "top" | "bottom",
  /// Top only: "notch" hangs from the screen edge, "pill" floats below it like the iPhone island.
  shape: "notch" as "notch" | "pill",
  transparency: "solid" as "solid" | "glass" | "clear",
  speed: "smooth" as "snappy" | "smooth" | "bouncy",
  volume: true,
  clipboard: true,
  media: true,
};

type Prefs = typeof DEFAULTS;

const SPEEDS: Record<Prefs["speed"], string> = {
  snappy: "220ms cubic-bezier(.2,.9,.3,1)",
  smooth: "450ms cubic-bezier(.32,.72,0,1)",
  bouncy: "650ms cubic-bezier(.34,1.56,.64,1)",
};
const BACKGROUNDS: Record<Prefs["transparency"], string> = {
  solid: "#000",
  glass: "rgba(18,18,20,.8)",
  clear: "rgba(64,64,68,.55)",
};
const GAP = 8; // px between a floating island and the screen edge / taskbar
const EAR = 10; // px: concave shoulders where a notch meets the screen edge
const BATTERY_FULL = 80; // % — "unplug" nudge while charging
const BATTERY_LOW = 20;
const ACCENT = "#ff375f"; // when there's no artwork to take a colour from

let audio: AudioContext | undefined;
/// Created on the click that starts a timer: WebView2 only lets audio play after a user gesture.
const unlockAudio = () => (audio ??= new AudioContext());
function beep() {
  if (!audio) return;
  for (const t of [0, 0.3, 0.6]) {
    const at = audio.currentTime + t;
    const osc = audio.createOscillator();
    const gain = audio.createGain();
    osc.frequency.value = 880;
    gain.gain.setValueAtTime(0.2, at);
    gain.gain.exponentialRampToValueAtTime(0.001, at + 0.25);
    osc.connect(gain).connect(audio.destination);
    osc.start(at);
    osc.stop(at + 0.25);
  }
}

const clock = (ms: number) => {
  const s = Math.max(0, Math.round(ms / 1000));
  const mmss = `${String(Math.floor(s / 60) % 60).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;
  return s >= 3600 ? `${Math.floor(s / 3600)}:${mmss}` : mmss;
};
const rate = (bps: number) => (bps >= 1e6 ? `${(bps / 1e6).toFixed(1)} MB/s` : `${Math.round(bps / 1e3)} KB/s`);

// WMO weather codes (open-meteo) → icon + label, by upper bound of each code range.
const WEATHER: [number, LucideIcon, string][] = [
  [0, Sun, "Clear"], [2, CloudSun, "Partly cloudy"], [3, Cloud, "Cloudy"], [48, CloudFog, "Fog"],
  [57, CloudDrizzle, "Drizzle"], [67, CloudRain, "Rain"], [77, CloudSnow, "Snow"], [82, CloudRain, "Showers"],
  [86, CloudSnow, "Snow showers"], [99, CloudLightning, "Thunderstorm"],
];

async function fetchWeather(): Promise<Weather> {
  const loc = await (await fetch("https://ipapi.co/json/")).json();
  const url = `https://api.open-meteo.com/v1/forecast?latitude=${loc.latitude}&longitude=${loc.longitude}&current=temperature_2m,weather_code`;
  const { current } = await (await fetch(url)).json();
  return { temp: Math.round(current.temperature_2m), code: current.weather_code };
}

function usePrefs() {
  const [prefs, setPrefs] = useState<Prefs>(() => ({ ...DEFAULTS, ...JSON.parse(localStorage.getItem("prefs") ?? "{}") }));
  useEffect(() => localStorage.setItem("prefs", JSON.stringify(prefs)), [prefs]);
  return [prefs, (p: Partial<Prefs>) => setPrefs((s) => ({ ...s, ...p }))] as const;
}

function usePoll<T>(fn: () => Promise<T>, ms: number) {
  const [value, setValue] = useState<T | null>(null);
  useEffect(() => {
    fn().then(setValue, () => {});
    const id = setInterval(() => fn().then(setValue, () => {}), ms);
    return () => clearInterval(id);
  }, []);
  return value;
}

/// The artwork's average colour pushed to full brightness; tints the glow, equalizer and accents.
function useAccent(art: string | null) {
  const [accent, setAccent] = useState(ACCENT);
  useEffect(() => {
    if (!art) return setAccent(ACCENT);
    const img = new Image();
    img.onload = () => {
      const ctx = document.createElement("canvas").getContext("2d")!;
      ctx.imageSmoothingQuality = "high";
      ctx.drawImage(img, 0, 0, 1, 1);
      const [r, g, b] = ctx.getImageData(0, 0, 1, 1).data;
      const max = Math.max(r, g, b);
      const k = 255 / max;
      setAccent(max < 32 ? "#fff" : `rgb(${(r * k) | 0},${(g * k) | 0},${(b * k) | 0})`);
    };
    img.src = art;
  }, [art]);
  return accent;
}

export default function App() {
  const [prefs, setPref] = usePrefs();
  // Rust: "open" once the cursor hits the screen edge over the island (and while it stays on it), else "out".
  // Hidden = thin line; shown = compact pill; a click on the shown pill expands it.
  const [shown, setShown] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const hover = shown && expanded;
  const [fullscreen, setFullscreen] = useState(false);
  // null: Home while there's media, else Controls.
  const [tab, setTab] = useState<Tab | null>(null);
  const [media, setMedia] = useState<Media>(null);
  const [toast, setToast] = useState<Toast | null>(null);
  // ponytail: in-memory only, history is lost on restart; persist to localStorage if that matters.
  const [notices, setNotices] = useState<Notice[]>([]);
  const status = usePoll(() => invoke<Status>("status"), 5000);
  const weather = usePoll(fetchWeather, 15 * 60_000);
  const now = usePoll(async () => new Date(), 5000) ?? new Date();
  const prefsRef = useRef(prefs);
  prefsRef.current = prefs;
  const hoverRef = useRef(false);
  hoverRef.current = hover;
  const shownRef = useRef(false);
  shownRef.current = shown;
  const [wapriv, setWapriv] = useState<WAPriv | null>(null);
  const [micMuted, setMicMuted] = useState<boolean | null>(null);
  // Keep Awake: null = off, Infinity = until turned off, else the ms timestamp it switches off.
  const [awake, setAwake] = useState<number | null>(null);
  const [timer, setTimer] = useState<Timer | null>(null);
  const [, tick] = useState(0);
  const inuse = usePoll(() => invoke<InUse>("inuse"), 1000);
  const toastTimer = useRef(0);

  const show = (t: Toast, ms: number) => {
    clearTimeout(toastTimer.current);
    setToast(t);
    // Actionable toasts wait while the island is open, so the cursor can reach their buttons.
    const hold = t.kind === "usb" || t.kind === "download";
    const hide = () => (hold && shownRef.current ? (toastTimer.current = window.setTimeout(hide, 500)) : setToast(null));
    toastTimer.current = window.setTimeout(hide, ms);
  };
  const eject = (d: Drive) =>
    invoke("eject", { letter: d.letter }).then(
      () => show({ kind: "info", icon: Usb, label: d.name, value: "Safe to remove", tone: "green", sticky: true }, 3500),
      () => show({ kind: "info", icon: Usb, label: d.name, value: "In use", tone: "red", sticky: true }, 3500),
    );

  useEffect(() => {
    invoke<Media>("media").then(setMedia);
    invoke<WAPriv>("wapriv").then(setWapriv);
    invoke<boolean>("mic", { toggle: false }).then(setMicMuted, () => {});
    if (prefsRef.current.greeting) invoke<string>("username").then((name) => show({ kind: "greeting", name }, 3500));
    const subs = [
      listen<"out" | "open">("hover", (e) => {
        setShown(e.payload === "open");
        if (e.payload === "out") {
          setTab(null);
          setExpanded(false);
        }
      }),
      listen<Media>("media", (e) => setMedia(e.payload)),
      listen<boolean>("fullscreen", (e) => setFullscreen(e.payload)),
      listen<{ level: number; muted: boolean }>("volume", ({ payload: v }) => {
        if (!prefsRef.current.volume) return;
        const muted = v.muted || v.level === 0;
        show({ kind: "info", icon: muted ? VolumeX : Volume2, label: "Volume", value: v.muted ? "Muted" : `${v.level}`, tone: v.muted ? "dim" : undefined, level: v.muted ? 0 : v.level }, 1800);
      }),
      listen<boolean>("mic", ({ payload: muted }) => {
        setMicMuted(muted);
        show({ kind: "info", icon: muted ? MicOff : Mic, label: "Microphone", value: muted ? "Muted" : "On", tone: muted ? "red" : "green" }, 1800);
      }),
      listen<{ key: string; on: boolean }>("lock", ({ payload: k }) =>
        show({ kind: "info", icon: k.key === "Caps Lock" ? ArrowBigUp : Hash, label: k.key, value: k.on ? "On" : "Off", tone: k.on ? "green" : "dim" }, 1500),
      ),
      listen<string>("clipboard", (e) => {
        if (prefsRef.current.clipboard) show({ kind: "clipboard", text: e.payload }, 1800);
      }),
      listen<Notif>("notification", (e) => {
        show({ kind: "notification", ...e.payload }, 6000);
        setNotices((l) => [{ ...e.payload, id: noticeId++, time: Date.now() }, ...l].slice(0, MAX_NOTICES));
      }),
      listen<Drive>("usb", (e) => show({ kind: "usb", drive: e.payload }, 8000)),
      listen<{ name: string; path: string }>("download", (e) => show({ kind: "download", ...e.payload }, 8000)),
      // Changes from the panel happen while it's open; anything else (Ctrl+Alt+B/S) gets a toast.
      listen<WAPriv>("wapriv", ({ payload: wa }) => {
        setWapriv(wa);
        if (!hoverRef.current) {
          show({ kind: "info", icon: EyeOff, label: "WA Privacy", value: wa.Enabled ? (wa.Spotlight ? "Spotlight" : "Blur") : "Off", tone: wa.Enabled ? "green" : "dim" }, 1800);
        }
      }),
    ];
    const noMenu = (e: MouseEvent) => e.preventDefault();
    document.addEventListener("contextmenu", noMenu);
    return () => {
      subs.forEach((s) => s.then((un) => un()));
      document.removeEventListener("contextmenu", noMenu);
    };
  }, []);

  useEffect(() => {
    invoke("set_capture_hidden", { hidden: prefs.hideCapture });
  }, [prefs.hideCapture]);

  useEffect(() => {
    invoke("keep_awake", { on: awake !== null });
    if (awake === null || awake === Infinity) return;
    const id = setTimeout(() => setAwake(null), awake - Date.now());
    return () => clearTimeout(id);
  }, [awake]);

  useEffect(() => {
    if (!timer || timer.paused !== null) return;
    const id = setInterval(() => {
      if (timer.total !== null && Date.now() - timer.start >= timer.total) {
        setTimer(null);
        beep();
        show({ kind: "info", icon: TimerIcon, label: timer.label, value: "Done", tone: "orange", sticky: true }, 8000);
      } else tick((n) => n + 1);
    }, 250);
    return () => clearInterval(id);
  }, [timer]);

  // Battery: one nudge when charging crosses the full mark, one when draining crosses the low mark.
  const lastStatus = useRef<Status | null>(null);
  useEffect(() => {
    const [p, s] = [lastStatus.current, status];
    lastStatus.current = s;
    if (p?.battery == null || s?.battery == null) return;
    if (s.charging && p.battery < BATTERY_FULL && s.battery >= BATTERY_FULL) {
      show({ kind: "info", icon: BatteryFull, label: `Battery ${s.battery}%`, value: "Unplug", tone: "green" }, 5000);
    } else if (!s.charging && p.battery > BATTERY_LOW && s.battery <= BATTERY_LOW) {
      show({ kind: "info", icon: BatteryLow, label: "Low Battery", value: `${s.battery}%`, tone: "red" }, 6000);
    }
  }, [status]);

  // Camera/mic: toast when an app starts using one; the pill keeps a dot while it's live.
  const lastInuse = useRef<InUse | null>(null);
  useEffect(() => {
    const [p, u] = [lastInuse.current, inuse];
    lastInuse.current = u;
    if (!p || !u) return;
    const cam = u.camera.find((a) => !p.camera.includes(a));
    const mic = u.mic.find((a) => !p.mic.includes(a));
    if (cam) show({ kind: "info", icon: Camera, label: "Camera", value: cam, tone: "green" }, 3000);
    else if (mic) show({ kind: "info", icon: Mic, label: "Microphone", value: mic, tone: "orange" }, 3000);
  }, [inuse]);

  const bottom = prefs.position === "bottom";
  const floating = bottom || prefs.shape === "pill";
  useEffect(() => {
    invoke("set_position", { bottom });
  }, [bottom]);

  // The island's size follows its content; measure it so width/height can be transitioned.
  const inner = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });
  useLayoutEffect(() => {
    const el = inner.current!;
    const ro = new ResizeObserver(() => setSize({ w: el.offsetWidth, h: el.offsetHeight }));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  // A second live activity splits off into a round bubble beside a compact pill:
  // the timer while media plays or a toast shows, or the playing media's art while a toast shows.
  const m = prefs.media ? media : null;
  const accent = useAccent(m?.art ?? null);
  const timerMain = !!timer && !m?.playing && !(toast && !hover);
  const aside = timer ? !timerMain : !!(m?.playing && toast);
  const split = aside && !hover && size.h < 60;
  const groupW = size.w + (split ? GAP + size.h : 0);
  useEffect(() => {
    const y = bottom ? window.innerHeight - size.h - GAP : floating ? GAP : 0;
    invoke("set_island", { x: (window.innerWidth - groupW) / 2, y, w: groupW, h: size.h });
  }, [size, groupW, bottom, floating]);

  const live = { camera: !!inuse?.camera.length, mic: !!inuse?.mic.length };
  const overPanel = toast && (toast.kind === "notification" || toast.kind === "usb" || toast.kind === "download" || (toast.kind === "info" && toast.sticky));
  let key: string;
  let view: ReactNode;
  if (toast && (overPanel || !hover)) {
    key = toast.kind;
    view = <ToastView toast={toast} media={m} eject={eject} dismiss={() => setToast(null)} hideWA={!!wapriv?.Enabled} />;
  } else if (hover) {
    const t = tab ?? (m ? "home" : "control");
    let body: ReactNode;
    if (t === "home") body = <Home media={m} now={now} weather={weather} timer={timer} openTimer={() => setTab("timer")} />;
    else if (t === "timer") body = <TimerPanel timer={timer} setTimer={setTimer} />;
    else if (t === "notifs") {
      body = (
        <NotificationsPanel
          notices={notices}
          now={now}
          hideWA={!!wapriv?.Enabled}
          dismiss={(id) => setNotices((l) => l.filter((n) => n.id !== id))}
          clear={() => setNotices([])}
        />
      );
    }
    else if (t === "settings") body = <SettingsPanel prefs={prefs} setPref={setPref} />;
    else if (t === "privacy" && wapriv) body = <PrivacyPanel wa={wapriv} />;
    else {
      body = (
        <ControlCenter
          wa={wapriv}
          micMuted={micMuted}
          toggleMic={() => invoke<boolean>("mic", { toggle: true }).then(setMicMuted)}
          awake={awake}
          // Off → on until turned off → on for an hour → off.
          cycleAwake={() => setAwake(awake === null ? Infinity : awake === Infinity ? Date.now() + 3600_000 : null)}
          timer={timer}
          eject={eject}
          open={setTab}
        />
      );
    }
    key = "expanded";
    view = (
      <Shell tab={t} setTab={setTab} status={status} live={live} privacy={!!wapriv} unread={notices.length}>
        <div className="view" key={t}>{body}</div>
      </Shell>
    );
  } else if (timerMain) {
    key = "timer-pill";
    view = <TimerPill timer={timer} live={live} />;
  } else if (m?.playing) {
    key = "media";
    view = <MediaPill media={m} live={live} />;
  } else {
    key = "idle";
    view = <IdlePill status={status} weather={weather} now={now} live={live} />;
  }

  // Hidden: the island shrinks into a thin line glued to the screen edge; toasts still pop it out. Only the
  // visual shrinks — the hit-rect sent to Rust keeps the measured size, so pushing to the edge reveals it.
  const peek = !shown && !toast;
  const transition = SPEEDS[prefs.speed];
  const T = parseInt(transition);
  // Floating compact pills travel through a round drop: line → drips off the edge as a ball → widens
  // into the pill, and back the same way. Notches and expanded panels morph directly.
  const droplet = floating && size.h < 60;
  const [phase, setPhase] = useState<"line" | "ball" | "full">("line");
  const [morph, setMorph] = useState<"drip" | "suck" | null>(null);
  const firstPeek = useRef(true);
  useEffect(() => {
    if (firstPeek.current) return void (firstPeek.current = false);
    const target = peek ? "line" : "full";
    if (!droplet || hover) {
      setPhase(target);
      setMorph(null);
      return;
    }
    setPhase("ball");
    // The neck pinches off as the line gathers into a ball, or stretches back up as the ball is sucked in.
    setMorph(peek ? null : "drip");
    const ids = [
      setTimeout(() => (setPhase(target), setMorph(peek ? "suck" : null)), T),
      setTimeout(() => setMorph(null), 2 * T),
    ];
    return () => ids.forEach(clearTimeout);
  }, [peek, hover]);
  const line = phase === "line";
  const ball = phase === "ball";
  const w = ball ? size.h : size.w;
  const h = line ? 4 : size.h;
  const r = line ? 2 : size.h < 60 ? size.h / 2 : 36;
  // Floating pills only round the edge-side corners as a line, so it sits flush with the screen.
  const radius = floating && !line ? r : bottom ? `${r}px ${r}px 0 0` : `0 0 ${r}px ${r}px`;
  const b = split ? size.h : 0; // bubble grows out of / back into the pill
  const ear = line ? 0 : EAR;
  // Direct morphs: into the line the pill drains toward the edge first, then narrows; back out it widens
  // first, then drops down.
  const lag = (p: string, late: boolean) => `${p} ${transition}${late && !droplet ? " 70ms" : ""}`;
  const grow = [lag("width", line), lag("height", !line), lag("border-radius", false), lag("margin-left", false), lag("box-shadow", false)].join(", ");
  const earMove = [lag("width", false), lag("height", false), lag("left", line)].join(", ");
  return (
    <div
      className={`stage ${bottom ? "bottom" : "top"} ${prefs.transparency}`}
      style={{
        paddingBottom: bottom && !line ? GAP : 0,
        paddingTop: !bottom && floating && !line ? GAP : 0,
        transition: `padding ${transition}${line || droplet ? "" : " 70ms"}`,
        ["--bg" as string]: BACKGROUNDS[prefs.transparency],
        ["--accent" as string]: accent,
      }}
    >
      <div className={`group ${morph ?? ""}`} style={{ opacity: fullscreen ? 0 : 1, ["--w" as string]: `${size.w}px`, ["--d" as string]: `${T}ms` }}>
        <div className="neck" style={{ left: w / 2 }} />
        {!floating && (
          <>
            <span className="ear left" style={{ left: -ear, width: ear, height: ear, transition: earMove }} />
            <span className="ear right" style={{ left: w, width: ear, height: ear, transition: earMove }} />
          </>
        )}
        <div
          className={`island main ${line || ball ? "peek" : ""}`}
          onClick={(e) => shown && !(e.target as Element).closest("button") && setExpanded(true)}
          onContextMenu={() => (setTab("control"), setExpanded(true))}
          style={{
            width: w,
            height: h,
            borderRadius: radius,
            // Expanded panels lift off the screen; compact pills stay flat like a notch.
            boxShadow: hover ? "0 18px 50px rgba(0,0,0,.55), 0 4px 14px rgba(0,0,0,.35)" : "0 0 0 rgba(0,0,0,0)",
            transition: grow,
          }}
        >
          <div className="content" ref={inner}>
            <div className="view" key={key}>{view}</div>
          </div>
        </div>
        <div
          className={`island bubble ${line || ball || !split ? "peek" : ""}`}
          style={{ width: b, height: line ? 4 : b, marginLeft: split ? GAP : 0, borderRadius: radius, transition: grow }}
        >
          {timer ? <TimerRing timer={timer} size={size.h} /> : m && <div className="disc"><Art media={m} size={size.h * 0.62} /></div>}
        </div>
      </div>
    </div>
  );
}

type Live = { camera: boolean; mic: boolean };

/// Camera (green) / mic (orange) in-use dots, like macOS's recording indicators.
function LiveDots({ live }: { live: Live }) {
  if (!live.camera && !live.mic) return null;
  return (
    <span className="live" aria-label={[live.camera && "Camera in use", live.mic && "Microphone in use"].filter(Boolean).join(", ")}>
      {live.camera && <span className="dot cam" />}
      {live.mic && <span className="dot mic" />}
    </span>
  );
}

function Battery({ status }: { status: Status | null }) {
  if (status?.battery == null) return null;
  const low = status.battery < BATTERY_LOW && !status.charging;
  return (
    <span className={`battery ${low ? "low" : status.charging ? "charging" : ""}`} aria-label={`Battery ${status.battery}%${status.charging ? ", charging" : ""}`}>
      {status.battery}%
      <span className="cell"><span style={{ width: `${status.battery}%` }} /></span>
      {status.charging && <Zap size={12} fill="currentColor" />}
    </span>
  );
}

function Meter({ value, className = "" }: { value: number; className?: string }) {
  return <span className={`meter ${className}`}><span style={{ width: `${Math.max(0, Math.min(100, value))}%` }} /></span>;
}

function IdlePill({ status, weather, now, live }: { status: Status | null; weather: Weather | null; now: Date; live: Live }) {
  const WeatherIcon = weather && (WEATHER.find(([max]) => weather.code <= max) ?? WEATHER[0])[1];
  const NetIcon = !status?.online ? WifiOff : status.wifi ? Wifi : Globe;
  return (
    <div className="pill idle">
      <div className="side">
        <LiveDots live={live} />
        <Battery status={status} />
        {status && <NetIcon size={14} className={status.online ? "" : "red"} />}
      </div>
      <div className="side">
        {WeatherIcon && (
          <>
            <WeatherIcon size={14} className="dim" />
            <span>{weather.temp}°</span>
          </>
        )}
        <span className="tabular">{now.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false })}</span>
      </div>
    </div>
  );
}

function Art({ media, size }: { media: NonNullable<Media>; size: number }) {
  const style = { width: size, height: size, borderRadius: Math.round(size / 4.5) };
  return media.art ? (
    <img className="art" src={media.art} style={style} alt="" />
  ) : (
    <span className="art ph" style={style}><Music size={size * 0.45} /></span>
  );
}

function Bars({ playing }: { playing: boolean }) {
  return (
    <div className={`bars ${playing ? "playing" : ""}`}>
      {[0, 1, 2, 3].map((i) => <span key={i} />)}
    </div>
  );
}

function MediaPill({ media, live }: { media: NonNullable<Media>; live: Live }) {
  return (
    <div className="pill media">
      <Art media={media} size={24} />
      <span className="title">{media.title}</span>
      <Bars playing={media.playing} />
      <LiveDots live={live} />
    </div>
  );
}

function timerElapsed(t: Timer) {
  return t.paused ?? Date.now() - t.start;
}
const timerLeft = (t: Timer) => clock(t.total === null ? timerElapsed(t) : t.total - timerElapsed(t));

function TimerPill({ timer, live }: { timer: Timer; live: Live }) {
  return (
    <div className="pill toast">
      <TimerIcon size={18} className="orange" />
      <span className="label">{timer.label}</span>
      <span className={`value tabular ${timer.paused !== null ? "dim" : "orange"}`}>{timerLeft(timer)}</span>
      <LiveDots live={live} />
    </div>
  );
}

/// Countdown: remaining fraction; stopwatch: sweeps once a minute.
function TimerRing({ timer, size }: { timer: Timer; size: number }) {
  const e = timerElapsed(timer);
  const frac = timer.total === null ? (e % 60_000) / 60_000 : 1 - e / timer.total;
  const [c, r, w] = [size / 2, size * 0.3, size * 0.09];
  const len = 2 * Math.PI * r;
  return (
    <svg width={size} height={size} className={timer.paused !== null ? "dim" : "orange"}>
      <circle cx={c} cy={c} r={r} fill="none" stroke="currentColor" strokeOpacity={0.25} strokeWidth={w} />
      <circle
        cx={c} cy={c} r={r} fill="none" stroke="currentColor" strokeWidth={w} strokeLinecap="round"
        strokeDasharray={len} strokeDashoffset={len * (1 - frac)} transform={`rotate(-90 ${c} ${c})`}
      />
    </svg>
  );
}

const TABS: { id: Tab; icon: LucideIcon; label: string }[] = [
  { id: "home", icon: House, label: "Home" },
  { id: "notifs", icon: Bell, label: "Notifications" },
  { id: "control", icon: LayoutGrid, label: "Controls" },
  { id: "timer", icon: TimerIcon, label: "Timer" },
  { id: "privacy", icon: EyeOff, label: "WhatsApp Privacy" },
];

/// Expanded island: tab bar on top (status + preferences on the right), the active tab below.
function Shell({ tab, setTab, status, live, privacy, unread, children }: { tab: Tab; setTab: (t: Tab) => void; status: Status | null; live: Live; privacy: boolean; unread: number; children: ReactNode }) {
  const tabButton = (id: Tab, Icon: LucideIcon, label: string) => (
    <button key={id} role="tab" aria-selected={tab === id} aria-label={label} title={label} className={`tab ${tab === id ? "active" : ""}`} onClick={() => setTab(id)}>
      <Icon size={17} />
      {id === "notifs" && unread > 0 && <span className="badge-count" aria-label={`${unread} notifications`}>{unread}</span>}
    </button>
  );
  return (
    <div className="panel shell">
      <div className="tabbar" role="tablist">
        {TABS.filter((t) => privacy || t.id !== "privacy").map((t) => tabButton(t.id, t.icon, t.label))}
        <span className="spacer" />
        <LiveDots live={live} />
        <Battery status={status} />
        {tabButton("settings", Settings, "Preferences")}
      </div>
      {children}
    </div>
  );
}

function Home({ media, now, weather, timer, openTimer }: { media: Media; now: Date; weather: Weather | null; timer: Timer | null; openTimer: () => void }) {
  const ctl = (action: string) => invoke("media_control", { action });
  const [, WeatherIcon, weatherLabel] = weather ? (WEATHER.find(([max]) => weather.code <= max) ?? WEATHER[0]) : [0, null, ""];
  return (
    <div className="home">
      <div className="player">
        {media ? (
          <>
            <div className="cover"><Art media={media} size={112} /></div>
            <div className="meta grow">
              <div className="title big">{media.title}</div>
              <div className="sub">{media.artist}</div>
              <div className="controls">
                <button aria-label="Previous" onClick={() => ctl("prev")}><SkipBack size={24} fill="currentColor" /></button>
                <button aria-label={media.playing ? "Pause" : "Play"} className="pp" onClick={() => ctl("toggle")}>
                  {media.playing ? <Pause size={24} fill="currentColor" /> : <Play size={24} fill="currentColor" />}
                </button>
                <button aria-label="Next" onClick={() => ctl("next")}><SkipForward size={24} fill="currentColor" /></button>
              </div>
            </div>
          </>
        ) : (
          <>
            <span className="art ph" style={{ width: 112, height: 112, borderRadius: 25 }}><Music size={40} /></span>
            <div className="meta grow">
              <div className="title big">Nothing playing</div>
              <div className="sub">Play something in any app and it shows up here.</div>
            </div>
          </>
        )}
      </div>
      <div className="divider" />
      <div className="day">
        <div className="weekday">{now.toLocaleDateString([], { weekday: "long" })}</div>
        <div className="date">{now.toLocaleDateString([], { day: "numeric", month: "short" })}</div>
        {weather && WeatherIcon && (
          <div className="card">
            <WeatherIcon size={20} className="blue" />
            <span className="meta">
              <span className="title">{weather.temp}°</span>
              <span className="sub">{weatherLabel}</span>
            </span>
          </div>
        )}
        {timer && (
          <button className="card" onClick={openTimer}>
            <TimerIcon size={20} className="orange" />
            <span className="meta grow">
              <span className="title">{timer.label}</span>
              <span className="sub tabular">{timerLeft(timer)}{timer.paused !== null && " · Paused"}</span>
            </span>
            <ChevronRight size={16} className="dim" />
          </button>
        )}
      </div>
    </div>
  );
}

const MODES = { Focus: 25, Break: 5 };
const MAX_MIN = 120;
const PX_PER_MIN = 12;

/// Minute ruler: drag, scroll or arrow keys; the centre tick is the value.
function Ruler({ value, onChange }: { value: number; onChange: (v: number) => void }) {
  const drag = useRef<{ x: number; v: number } | null>(null);
  const set = (v: number) => onChange(Math.min(MAX_MIN, Math.max(1, Math.round(v))));
  const ticks: number[] = [];
  for (let n = Math.max(1, value - 30); n <= Math.min(MAX_MIN, value + 30); n++) ticks.push(n);
  return (
    <div
      className="ruler"
      role="slider"
      tabIndex={0}
      aria-label="Minutes"
      aria-valuemin={1}
      aria-valuemax={MAX_MIN}
      aria-valuenow={value}
      onKeyDown={(e) => {
        if (e.key === "ArrowLeft" || e.key === "ArrowDown") set(value - 1);
        else if (e.key === "ArrowRight" || e.key === "ArrowUp") set(value + 1);
      }}
      onWheel={(e) => set(value + Math.sign(e.deltaY || e.deltaX))}
      onPointerDown={(e) => {
        e.currentTarget.setPointerCapture(e.pointerId);
        drag.current = { x: e.clientX, v: value };
      }}
      onPointerMove={(e) => drag.current && set(drag.current.v - (e.clientX - drag.current.x) / PX_PER_MIN)}
      onPointerUp={() => (drag.current = null)}
    >
      {ticks.map((n) => (
        <span key={n} className={`tick ${n % 5 === 0 ? "major" : ""} ${n === value ? "current" : ""}`} style={{ left: `calc(50% + ${(n - value) * PX_PER_MIN}px)` }}>
          {(n === value || (n % 5 === 0 && Math.abs(n - value) > 2)) && <b>{n}</b>}
        </span>
      ))}
    </div>
  );
}

function TimerPanel({ timer, setTimer }: { timer: Timer | null; setTimer: (t: Timer | null) => void }) {
  const [mode, setMode] = useState<keyof typeof MODES>("Focus");
  const [min, setMin] = useState(MODES.Focus);
  const start = (label: string, total: number | null) => {
    unlockAudio();
    setTimer({ label, total, start: Date.now(), paused: null });
  };
  if (timer) {
    const paused = timer.paused !== null;
    const togglePause = () =>
      setTimer(paused ? { ...timer, start: Date.now() - timer.paused!, paused: null } : { ...timer, paused: Date.now() - timer.start });
    return (
      <div className="timer">
        <div className="chips">
          <span className="chip active">{timer.label}</span>
          {paused && <span className="chip">Paused</span>}
        </div>
        <div className="progress">
          {timer.total !== null && <Meter value={(1 - timerElapsed(timer) / timer.total) * 100} className="wide accent" />}
        </div>
        <div className="timer-foot">
          <button className="round accent" aria-label={paused ? "Resume" : "Pause"} onClick={togglePause}>
            {paused ? <Play size={20} fill="currentColor" /> : <Pause size={20} fill="currentColor" />}
          </button>
          <button className="round" aria-label="Stop" onClick={() => setTimer(null)}><Square size={18} fill="currentColor" /></button>
          <span className="spacer" />
          <span className={`big-clock ${paused ? "dim" : ""}`}>{timerLeft(timer)}</span>
        </div>
      </div>
    );
  }
  return (
    <div className="timer">
      <div className="chips" role="radiogroup" aria-label="Mode">
        {(Object.keys(MODES) as (keyof typeof MODES)[]).map((k) => (
          <button key={k} role="radio" aria-checked={mode === k} className={`chip ${mode === k ? "active" : ""}`} onClick={() => (setMode(k), setMin(MODES[k]))}>
            {k}
          </button>
        ))}
      </div>
      <Ruler value={min} onChange={setMin} />
      <div className="timer-foot">
        <button className="round accent" aria-label={`Start ${mode}`} onClick={() => start(mode, min * 60_000)}><Play size={20} fill="currentColor" /></button>
        <button className="round" aria-label="Start stopwatch" title="Stopwatch" onClick={() => start("Stopwatch", null)}><TimerIcon size={20} /></button>
        <span className="spacer" />
        <span className="big-clock">{clock(min * 60_000)}</span>
      </div>
    </div>
  );
}

const POWER: { id: string; label: string; icon: LucideIcon; color: string; confirm?: boolean }[] = [
  { id: "lock", label: "Lock", icon: Lock, color: "#0a84ff" },
  { id: "sleep", label: "Sleep", icon: Moon, color: "#5e5ce6" },
  { id: "restart", label: "Restart", icon: RotateCcw, color: "#30d158", confirm: true },
  { id: "shutdown", label: "Shut Down", icon: Power, color: "#ff453a", confirm: true },
  { id: "screenshot", label: "Screenshot", icon: Scissors, color: "#bf5af2" },
  { id: "wifi", label: "Wi-Fi", icon: Wifi, color: "#64d2ff" },
  { id: "settings", label: "Settings", icon: Settings, color: "#aeaeb2" },
  { id: "bluetooth", label: "Bluetooth", icon: Bluetooth, color: "#0a84ff" },
];

type CCProps = {
  wa: WAPriv | null;
  micMuted: boolean | null;
  toggleMic: () => void;
  awake: number | null;
  cycleAwake: () => void;
  timer: Timer | null;
  eject: (d: Drive) => void;
  open: (t: Tab) => void;
};

function ControlCenter({ wa, micMuted, toggleMic, awake, cycleAwake, timer, eject, open }: CCProps) {
  // Restart/Shut Down need a second tap within 3s so a stray click can't kill the session.
  const [armed, setArmed] = useState<string | null>(null);
  useEffect(() => {
    if (!armed) return;
    const id = setTimeout(() => setArmed(null), 3000);
    return () => clearTimeout(id);
  }, [armed]);
  const stats = usePoll(() => invoke<Stats>("stats"), 1000);
  const drives = usePoll(() => invoke<Drive[]>("drives"), 2000);
  const monitors = usePoll(() => invoke<Monitor[]>("monitors"), 60_000);
  const power = (a: (typeof POWER)[number]) => {
    if (a.confirm && armed !== a.id) return setArmed(a.id);
    setArmed(null);
    invoke("power_action", { action: a.id }).catch(console.error);
  };
  const items: { id: string; label: string; icon: LucideIcon; color: string; on?: boolean; run: () => void }[] = [
    ...POWER.map((a) => ({ ...a, label: armed === a.id ? "Tap again" : a.label, on: armed === a.id, run: () => power(a) })),
    { id: "mic", label: micMuted ? "Mic Off" : "Mic", icon: micMuted ? MicOff : Mic, color: micMuted ? "#ff453a" : "#30d158", on: !!micMuted, run: toggleMic },
    { id: "awake", label: awake === null ? "Awake" : awake === Infinity ? "Awake ∞" : "Awake 1h", icon: Coffee, color: "#ffd60a", on: awake !== null, run: cycleAwake },
    { id: "timer", label: "Timer", icon: TimerIcon, color: "#ff9f0a", on: !!timer, run: () => open("timer") },
    { id: "downloads", label: "Downloads", icon: FolderDown, color: "#63e6e2", run: () => invoke("power_action", { action: "downloads" }) },
  ];
  return (
    <div className="control">
      {stats && (
        <div className="stats">
          <span className="stat">CPU<b>{stats.cpu}%</b></span>
          <span className="stat">Memory<b>{stats.ram}%</b></span>
          <span className="stat">Download<b>{rate(stats.down)}</b></span>
          <span className="stat">Upload<b>{rate(stats.up)}</b></span>
        </div>
      )}
      <div className="sliders">
        <VolumeSlider />
        {monitors?.map((m) => <BrightnessSlider key={m.id} monitor={m} />)}
      </div>
      <div className="tiles">
        {items.map((a) => (
          <button key={a.id} className={`tile ${a.on ? "on" : ""}`} onClick={a.run} style={{ ["--c" as string]: a.color }}>
            <span className="circle"><a.icon size={19} /></span>
            <span className="label">{a.label}</span>
          </button>
        ))}
      </div>
      <div className="rows">
        {drives?.map((d) => (
          <div key={d.letter} className="card">
            <span className="circle small" style={{ ["--c" as string]: "#63e6e2" }}><Usb size={16} /></span>
            <span className="meta grow">
              <span className="title">{d.name}</span>
              <span className="sub">USB drive ({d.letter}:)</span>
            </span>
            <button className="pill-btn" onClick={() => eject(d)}>Eject</button>
          </div>
        ))}
        {wa && (
          <div className="card">
            <button className="card-open" onClick={() => open("privacy")}>
              <span className="circle small" style={{ ["--c" as string]: "#30d158" }}><EyeOff size={16} /></span>
              <span className="meta grow">
                <span className="title">WhatsApp Privacy</span>
                <span className="sub">{wa.Enabled ? `Blur ${wa.Blur}%${wa.Spotlight ? " · Spotlight" : ""}` : "Off"}</span>
              </span>
              <ChevronRight size={16} className="dim" />
            </button>
            <button role="switch" aria-checked={wa.Enabled} aria-label="WhatsApp blur" className={`toggle ${wa.Enabled ? "on" : ""}`}
              onClick={() => setWAPriv({ ...wa, Enabled: !wa.Enabled })} />
          </div>
        )}
      </div>
    </div>
  );
}

function Slider({ icon: Icon, label, value, min = 0, max = 100, onChange, onIcon, iconLabel }: {
  icon: LucideIcon; label: string; value: number; min?: number; max?: number;
  onChange: (v: number) => void; onIcon?: () => void; iconLabel?: string;
}) {
  const pct = Math.round(((value - min) * 100) / (max - min));
  return (
    <div className="slider">
      {onIcon ? (
        <button className="slider-icon" onClick={onIcon} aria-label={iconLabel}><Icon size={17} /></button>
      ) : (
        <span className="slider-icon"><Icon size={17} /></span>
      )}
      <span className="grow">
        <span className="slider-label">{label}</span>
        <input type="range" min={min} max={max} value={value} aria-label={label}
          style={{ ["--p" as string]: `${pct}%` }} onChange={(e) => onChange(Number(e.target.value))} />
      </span>
      <span className="value tabular">{pct}%</span>
    </div>
  );
}

/// Default output device; the `volume` event keeps it in sync with keys and the Windows mixer.
function VolumeSlider() {
  const [vol, setVol] = useState<{ level: number; muted: boolean } | null>(null);
  useEffect(() => {
    invoke<typeof vol>("volume", {}).then(setVol).catch(console.error);
    const un = listen<NonNullable<typeof vol>>("volume", (e) => setVol(e.payload));
    return () => void un.then((f) => f());
  }, []);
  if (!vol) return null;
  const muted = vol.muted || vol.level === 0;
  return (
    <Slider icon={muted ? VolumeX : Volume2} label="Volume" value={vol.muted ? 0 : vol.level}
      iconLabel={vol.muted ? "Unmute" : "Mute"}
      onIcon={() => invoke<typeof vol>("volume", { muted: !vol.muted }).then(setVol).catch(console.error)}
      onChange={(level) => { setVol({ level, muted: false }); invoke("volume", { level, muted: false }).catch(console.error); }} />
  );
}

/// Built-in panel (WMI) or external monitor (DDC/CI); writes are debounced since each one takes tens of ms.
function BrightnessSlider({ monitor }: { monitor: Monitor }) {
  const [value, setValue] = useState(monitor.value);
  const pending = useRef(0);
  const change = (v: number) => {
    setValue(v);
    clearTimeout(pending.current);
    pending.current = window.setTimeout(() => invoke("set_brightness", { id: monitor.id, value: v }).catch(console.error), 150);
  };
  return <Slider icon={Sun} label={monitor.name} value={value} min={monitor.min} max={monitor.max} onChange={change} />;
}

function Toggle({ on, onChange, label }: { on: boolean; onChange: (v: boolean) => void; label: string }) {
  return (
    <label className="setting">
      <span>{label}</span>
      <button role="switch" aria-checked={on} className={`toggle ${on ? "on" : ""}`} onClick={() => onChange(!on)} />
    </label>
  );
}

function Segmented<T extends string>({ label, value, options, onChange }: { label: string; value: T; options: T[]; onChange: (v: T) => void }) {
  return (
    <div className="setting stack">
      <span>{label}</span>
      <div className="segmented" role="radiogroup" aria-label={label}>
        {options.map((o) => (
          <button key={o} role="radio" aria-checked={o === value} className={o === value ? "active" : ""} onClick={() => onChange(o)}>
            {o[0].toUpperCase() + o.slice(1)}
          </button>
        ))}
      </div>
    </div>
  );
}

function SettingsPanel({ prefs, setPref }: { prefs: Prefs; setPref: (p: Partial<Prefs>) => void }) {
  const [startup, setStartup] = useState(false);
  useEffect(() => {
    isEnabled().then(setStartup, () => {});
  }, []);
  const toggleStartup = async (v: boolean) => {
    await (v ? enable() : disable());
    setStartup(await isEnabled());
  };
  return (
    <div className="settings">
      <div>
        <h3>General</h3>
        <div className="list">
          <Toggle label="Run on Startup" on={startup} onChange={toggleStartup} />
          <Toggle label="Boot Greeting" on={prefs.greeting} onChange={(greeting) => setPref({ greeting })} />
          <Toggle label="Hide from Screen Share" on={prefs.hideCapture} onChange={(hideCapture) => setPref({ hideCapture })} />
        </div>
        <h3>Modules</h3>
        <div className="list">
          <Toggle label="Volume Monitor" on={prefs.volume} onChange={(volume) => setPref({ volume })} />
          <Toggle label="Clipboard Monitor" on={prefs.clipboard} onChange={(clipboard) => setPref({ clipboard })} />
          <Toggle label="Media Player" on={prefs.media} onChange={(media) => setPref({ media })} />
        </div>
      </div>
      <div>
        <h3>Appearance</h3>
        <div className="list">
          <Segmented label="Position" value={prefs.position} options={["top", "bottom"]} onChange={(position) => setPref({ position })} />
          <Segmented label="Style" value={prefs.shape} options={["notch", "pill"]} onChange={(shape) => setPref({ shape })} />
          <Segmented label="Transparency" value={prefs.transparency} options={["solid", "glass", "clear"]} onChange={(transparency) => setPref({ transparency })} />
          <Segmented label="Animation Speed" value={prefs.speed} options={["snappy", "smooth", "bouncy"]} onChange={(speed) => setPref({ speed })} />
        </div>
      </div>
    </div>
  );
}

const SPOT_SIZES = { small: [400, 140], medium: [560, 200], large: [800, 300] } as const;

function PrivacyPanel({ wa }: { wa: WAPriv }) {
  const size = (Object.keys(SPOT_SIZES) as (keyof typeof SPOT_SIZES)[]).find((k) => SPOT_SIZES[k][0] === wa.HoleW) ?? "medium";
  return (
    <div className="settings">
      <div>
        <h3>WhatsApp Privacy</h3>
        <div className="list">
          <Toggle label="Blur WhatsApp" on={wa.Enabled} onChange={(Enabled) => setWAPriv({ ...wa, Enabled })} />
          <Toggle label="Spotlight around cursor" on={wa.Spotlight} onChange={(Spotlight) => setWAPriv({ ...wa, Spotlight })} />
        </div>
        <p className="hint">Hold <kbd>Right Ctrl</kbd> to reveal · <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>B</kbd> blur · <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>S</kbd> spotlight</p>
      </div>
      <div>
        <h3>Tuning</h3>
        <div className="list">
          <div className="setting stack">
            <span>Blur Strength<span className="value">{wa.Blur}%</span></span>
            <input type="range" min={0} max={100} step={5} value={wa.Blur} aria-label="Blur strength"
              style={{ ["--p" as string]: `${wa.Blur}%` }} onChange={(e) => setWAPriv({ ...wa, Blur: Number(e.target.value) })} />
          </div>
          <Segmented label="Spotlight Size" value={size} options={["small", "medium", "large"]}
            onChange={(k) => setWAPriv({ ...wa, HoleW: SPOT_SIZES[k][0], HoleH: SPOT_SIZES[k][1] })} />
        </div>
      </div>
    </div>
  );
}

/// Notification history: click a card to expand it, Open launches the source app, × dismisses.
function NotificationsPanel({ notices, now, hideWA, dismiss, clear }: { notices: Notice[]; now: Date; hideWA: boolean; dismiss: (id: number) => void; clear: () => void }) {
  const [open, setOpen] = useState<number | null>(null);
  if (!notices.length) {
    return (
      <div className="notices-empty">
        <Bell size={26} />
        <span>No notifications</span>
      </div>
    );
  }
  return (
    <div className="notices">
      <div className="notices-head">
        <h3>Notifications</h3>
        <button className="pill-btn" onClick={clear}>Clear all</button>
      </div>
      <div className="notice-list">
        {notices.map((n) => {
          const masked = hideWA && /whatsapp/i.test(n.app);
          const min = Math.floor((now.getTime() - n.time) / 60_000);
          const ago = min < 1 ? "now" : min < 60 ? `${min}m` : `${Math.floor(min / 60)}h`;
          const expanded = open === n.id;
          return (
            <div key={n.id} className={`card notice ${expanded ? "open" : ""}`}>
              <button className="notice-main" aria-expanded={expanded} onClick={() => setOpen(expanded ? null : n.id)}>
                {n.icon ? <img className="art" src={n.icon} style={{ width: 32, height: 32, borderRadius: 8 }} alt="" /> : <span className="art ph" style={{ width: 32, height: 32, borderRadius: 8 }}><Bell size={16} /></span>}
                <span className="meta grow">
                  <span className="notice-top"><span className="ellipsis">{n.app}</span><span className="tabular">{ago}</span></span>
                  <span className="title">{masked ? "New message" : n.title}</span>
                  {!masked && n.body && <span className="notice-body">{n.body}</span>}
                </span>
              </button>
              <div className="notice-actions">
                {n.aumid && (
                  <button className="pill-btn" onClick={() => { openPath(`shell:AppsFolder\\${n.aumid}`, false); dismiss(n.id); }}>Open</button>
                )}
                <button className="icon-btn" aria-label={`Dismiss ${n.app} notification`} onClick={() => dismiss(n.id)}><X size={14} /></button>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function ToastView({ toast, media, eject, dismiss, hideWA }: { toast: Toast; media: Media; eject: (d: Drive) => void; dismiss: () => void; hideWA: boolean }) {
  switch (toast.kind) {
    case "info":
      return toast.level === undefined ? (
        <div className="pill toast">
          <toast.icon size={18} className={toast.tone} />
          <span className="label">{toast.label}</span>
          <span className={`value ellipsis ${toast.tone ?? ""}`}>{toast.value}</span>
        </div>
      ) : (
        <div className="pill toast hud">
          <toast.icon size={18} className={toast.tone} />
          <Meter value={toast.level} />
          <span className="value dim tabular">{toast.value}</span>
        </div>
      );
    case "usb":
      return (
        <div className="pill toast">
          <Usb size={18} className="teal" />
          <span className="label ellipsis">{toast.drive.name}</span>
          <span className="sub">{toast.drive.letter}:</span>
          <button className="pill-btn" onClick={() => eject(toast.drive)}>Eject</button>
        </div>
      );
    case "download":
      return (
        <div className="pill toast">
          <Download size={18} className="green" />
          <span className="label ellipsis">{toast.name}</span>
          <button className="pill-btn" onClick={() => { openPath(toast.path, false); dismiss(); }}>Open</button>
          <button className="pill-btn" onClick={() => { openPath(toast.path, true); dismiss(); }}>Show</button>
        </div>
      );
    case "clipboard":
      return (
        <div className="pill toast">
          <Clipboard size={18} className="dim" />
          <span className="label">Copied</span>
          <span className="sub ellipsis">{toast.text}</span>
        </div>
      );
    case "greeting": {
      const h = new Date().getHours();
      const part = h < 12 ? "morning" : h < 18 ? "afternoon" : "evening";
      return (
        <div className="pill toast">
          <Hand size={18} className="yellow" />
          <span className="label">Good {part}{toast.name && `, ${toast.name}`}</span>
        </div>
      );
    }
    case "notification": {
      const masked = hideWA && /whatsapp/i.test(toast.app);
      return (
        <div className="panel notification">
          {media && (
            <div className="row">
              <Art media={media} size={40} />
              <div className="meta">
                <div className="title">{media.title}</div>
                <div className="sub">{media.artist}</div>
              </div>
            </div>
          )}
          <div className="row">
            {toast.icon ? <img className="art" src={toast.icon} style={{ width: 40, height: 40, borderRadius: 10 }} alt="" /> : <span className="art ph" style={{ width: 40, height: 40, borderRadius: 10 }}><Bell size={20} /></span>}
            <div className="meta">
              <div className="title">{toast.app}</div>
              <div className="sub">{masked ? "New message" : [toast.title, toast.body].filter(Boolean).join(" — ")}</div>
            </div>
          </div>
        </div>
      );
    }
  }
}
