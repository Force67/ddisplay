import { Transport } from './transport.js';
import { H264Decoder } from './decoder.js';
import { Renderer } from './renderer.js';
import { InputHandler } from './input.js';

// Protocol constants
const SRV_VIDEO_FRAME   = 0x01;
const SRV_CURSOR_UPDATE = 0x02;
const SRV_SESSION_INFO  = 0x03;
const CLI_CLIENT_READY  = 0x14;

// DOM
const canvas          = document.getElementById('display');
const videoEl         = document.getElementById('video');
const overlay         = document.getElementById('overlay');
const connectionLabel = document.getElementById('connection-status');
const statFps         = document.getElementById('stat-fps');
const statLatency     = document.getElementById('stat-latency');
const statResolution  = document.getElementById('stat-resolution');
const sessionSwitcher = document.getElementById('session-switcher');
const helperSession   = document.getElementById('helper-session');

// Modules
const transport = new Transport();
const decoder   = new H264Decoder(videoEl, 30);
const renderer  = new Renderer(canvas);
const input     = new InputHandler(canvas);

const SESSION_PORTS = {
    virtual: '9550',
    physical: '9551',
};
const currentSession = location.port === SESSION_PORTS.physical ? 'physical' : 'virtual';

// Input -> transport
input.onSend = (buf) => transport.send(buf);

// Decoder -> renderer (MSE updates the <video>, we draw it to canvas)
decoder.onFrame = (video) => {
    renderer.drawVideoFrame(video);
    if (renderer.remoteWidth > 0 && renderer.remoteHeight > 0) {
        input.setRemoteSize(renderer.remoteWidth, renderer.remoteHeight);
    }
};

// Also render on requestAnimationFrame for smooth playback
function renderLoop() {
    if (videoEl.readyState >= 2 && videoEl.videoWidth > 0) {
        renderer.drawVideoFrame(videoEl);
        if (renderer.remoteWidth > 0 && renderer.remoteHeight > 0) {
            input.setRemoteSize(renderer.remoteWidth, renderer.remoteHeight);
        }
    }
    requestAnimationFrame(renderLoop);
}
requestAnimationFrame(renderLoop);

// Transport -> decoder
let msgCount = 0;
let frameIndex = 0;

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
    }
};

function handleVideoFrame(view, data) {
    const keyframe = view.getUint8(1) !== 0;
    const width    = view.getUint16(10, true);
    const height   = view.getUint16(12, true);
    const payload  = new Uint8Array(data, 14);

    const timestamp = frameIndex * 33333;
    frameIndex++;

    decoder.decode(payload, keyframe, timestamp, width, height);
}

function handleCursorUpdate(view) {
    const x = view.getUint16(1, true);
    const y = view.getUint16(3, true);
    const visible = view.getUint8(5) !== 0;
    renderer.setCursor(x, y, visible);
}

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

// Connection state
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

// Stats
function updateStats() {
    statFps.textContent = `${renderer.fps} FPS`;
    statLatency.textContent = `q:${decoder.queueSize}`;
    if (renderer.remoteWidth > 0) {
        statResolution.textContent = `${renderer.remoteWidth}\u00d7${renderer.remoteHeight}`;
    }
    setTimeout(updateStats, 500);
}
updateStats();

function renderSessionSwitcher() {
    const sessions = [
        { id: 'virtual', label: 'Virtual', port: SESSION_PORTS.virtual },
        { id: 'physical', label: 'Physical', port: SESSION_PORTS.physical },
    ];

    helperSession.textContent = `session:${currentSession}`;
    sessionSwitcher.replaceChildren(
        ...sessions.map((session) => {
            const link = document.createElement('a');
            link.className = `session-button${session.id === currentSession ? ' active' : ''}`;
            link.href = `${location.protocol}//${location.hostname}:${session.port}/`;
            link.textContent = session.label;
            return link;
        }),
    );
}
renderSessionSwitcher();

// Fullscreen
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

canvas.addEventListener('dblclick', () => toggleFullscreen());

// Auto-connect
const wsUrl = `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/ws`;
transport.connect(wsUrl);
