import { Transport } from './transport.js';
import { H264Decoder } from './decoder.js';
import { Renderer } from './renderer.js';
import { InputHandler } from './input.js';

// ---- Protocol constants ----
const SRV_VIDEO_FRAME   = 0x01;
const SRV_CURSOR_UPDATE = 0x02;
const SRV_SESSION_INFO  = 0x03;
const CLI_CLIENT_READY  = 0x14;

// ---- DOM references ----
const canvas          = /** @type {HTMLCanvasElement} */ (document.getElementById('display'));
const overlay         = document.getElementById('overlay');
const connectionLabel = document.getElementById('connection-status');
const statFps         = document.getElementById('stat-fps');
const statLatency     = document.getElementById('stat-latency');
const statResolution  = document.getElementById('stat-resolution');

// ---- Module instances ----
const transport = new Transport();
const decoder   = new H264Decoder();
const renderer  = new Renderer(canvas);
const input     = new InputHandler(canvas);

// ---- Wiring: input -> transport ----
input.onSend = (buf) => transport.send(buf);

// ---- Wiring: transport -> decoder -> renderer ----
let msgCount = 0;

transport.onMessage = (data) => {
    const view = new DataView(data);
    const type = view.getUint8(0);
    msgCount++;

    if (msgCount <= 5) {
        console.log(`[ws] msg #${msgCount}: type=0x${type.toString(16)}, size=${data.byteLength}`);
    }

    switch (type) {
        case SRV_VIDEO_FRAME:
            handleVideoFrame(view, data);
            break;
        case SRV_CURSOR_UPDATE:
            handleCursorUpdate(view);
            break;
        case SRV_SESSION_INFO:
            handleSessionInfo(data);
            break;
        default:
            console.warn(`[ws] unknown message type: 0x${type.toString(16)}`);
            break;
    }
};

/**
 * Parse and decode a VideoFrame message.
 * Layout: [type:u8][keyframe:u8][pts:u64 LE][width:u16 LE][height:u16 LE][data...]
 */
let frameIndex = 0;

function handleVideoFrame(view, data) {
    const keyframe  = view.getUint8(1) !== 0;
    const width     = view.getUint16(10, true);
    const height    = view.getUint16(12, true);
    const payload   = new Uint8Array(data, 14);

    // WebCodecs needs timestamps in microseconds. The server sends a frame
    // counter, so we generate monotonic timestamps locally.
    const timestamp = frameIndex * 33333; // ~30fps in microseconds
    frameIndex++;

    decoder.decode(payload, keyframe, timestamp, width, height);
}

/**
 * Handle a CursorUpdate message.
 * Layout: [type:u8][x:u16 LE][y:u16 LE][visible:u8]
 */
function handleCursorUpdate(view) {
    const visible = view.getUint8(5) !== 0;
    canvas.style.cursor = visible ? 'default' : 'none';
}

/**
 * Handle a SessionInfo message.
 * Layout: [type:u8][json_payload...]
 */
function handleSessionInfo(data) {
    const jsonBytes = new Uint8Array(data, 1);
    const text = new TextDecoder().decode(jsonBytes);
    try {
        const info = JSON.parse(text);
        console.log('[session]', info);
    } catch (e) {
        console.warn('[session] invalid JSON:', e);
    }
}

// ---- Decoder -> Renderer ----
decoder.onFrame = (frame) => {
    renderer.drawFrame(frame);

    // Keep input handler aware of the remote resolution.
    if (renderer.remoteWidth > 0 && renderer.remoteHeight > 0) {
        input.setRemoteSize(renderer.remoteWidth, renderer.remoteHeight);
    }
};

// ---- Connection state ----
transport.onStateChange = (state) => {
    switch (state) {
        case 'connecting':
            connectionLabel.textContent = 'Connecting...';
            overlay.classList.remove('hidden');
            break;
        case 'connected':
            connectionLabel.textContent = 'Connected';
            overlay.classList.add('hidden');
            sendClientReady();
            break;
        case 'disconnected':
            connectionLabel.textContent = 'Disconnected \u2014 reconnecting...';
            overlay.classList.remove('hidden');
            break;
    }
};

function sendClientReady() {
    const buf = new ArrayBuffer(1);
    new DataView(buf).setUint8(0, CLI_CLIENT_READY);
    transport.send(buf);
}

// ---- Stats update loop ----
let lastStatsUpdate = 0;

function updateStats() {
    const now = performance.now();
    if (now - lastStatsUpdate > 500) {
        lastStatsUpdate = now;
        statFps.textContent = `${renderer.fps} FPS`;
        statLatency.textContent = `q:${decoder.queueSize}`;
        if (renderer.remoteWidth > 0) {
            statResolution.textContent = `${renderer.remoteWidth}\u00d7${renderer.remoteHeight}`;
        }
    }
    requestAnimationFrame(updateStats);
}

requestAnimationFrame(updateStats);

// ---- Fullscreen ----
function toggleFullscreen() {
    if (!document.fullscreenElement) {
        document.documentElement.requestFullscreen().catch(() => {});
    } else {
        document.exitFullscreen().catch(() => {});
    }
}

window.addEventListener('keydown', (e) => {
    if (e.key === 'F11') {
        e.preventDefault();
        toggleFullscreen();
    }
});

canvas.addEventListener('dblclick', () => {
    toggleFullscreen();
});

// ---- Auto-connect ----
const wsProtocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
const wsUrl = `${wsProtocol}//${location.host}/ws`;
transport.connect(wsUrl);
