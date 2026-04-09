import { Transport } from './transport.js';
import { H264Decoder } from './decoder.js';
import { Renderer } from './renderer.js';
import { InputHandler } from './input.js';

const SRV_VIDEO_FRAME = 0x01;
const SRV_CURSOR_UPDATE = 0x02;
const SRV_SESSION_INFO = 0x03;
const CLI_CLIENT_READY = 0x14;

const SESSION_PORTS = {
    virtual: '9550',
    physical: '9551',
};
const SIDEBAR_STATE_KEY = 'ddisplay.sidebarCollapsed';

const appShell = document.getElementById('app-shell');
const canvas = document.getElementById('display');
const videoEl = document.getElementById('video');
const overlay = document.getElementById('overlay');
const connectionLabel = document.getElementById('connection-status');
const statFps = document.getElementById('stat-fps');
const statLatency = document.getElementById('stat-latency');
const statResolution = document.getElementById('stat-resolution');
const sessionSwitcher = document.getElementById('session-switcher');
const helperSession = document.getElementById('helper-session');
const warningBanner = document.getElementById('warning-banner');
const statusPanel = document.getElementById('status-panel');
const btnSidebarCollapse = document.getElementById('btn-sidebar-collapse');
const btnSidebarPeek = document.getElementById('btn-sidebar-peek');
const btnReconnect = document.getElementById('btn-reconnect');
const btnReadOnly = document.getElementById('btn-readonly');
const bitrateSelect = document.getElementById('bitrate-select');
const btnRestart = document.getElementById('btn-restart');
const btnReleaseKeys = document.getElementById('btn-release-keys');
const btnReleaseMouse = document.getElementById('btn-release-mouse');
const btnReleaseAll = document.getElementById('btn-release-all');
const btnSendSuper = document.getElementById('btn-send-super');
const btnPullClipboard = document.getElementById('btn-pull-clipboard');
const pasteText = document.getElementById('paste-text');
const btnPasteHostClipboard = document.getElementById('btn-paste-host-clipboard');
const btnSendText = document.getElementById('btn-send-text');
const btnClearText = document.getElementById('btn-clear-text');

const pageParams = new URLSearchParams(location.search);
const currentSession = location.port === SESSION_PORTS.physical ? 'physical' : 'virtual';

let readOnly = pageParams.get('readonly') === '1';
let statusPollTimer = null;
let msgCount = 0;
let frameIndex = 0;
let serverStatus = null;
let sidebarCollapsed = false;

const transport = new Transport();
const decoder = new H264Decoder(videoEl, 30);
const renderer = new Renderer(canvas);
const input = new InputHandler(canvas);

input.setReadOnly(readOnly);
input.onSend = (buf) => transport.send(buf);

decoder.onFrame = (video) => {
    renderer.drawVideoFrame(video);
    if (renderer.remoteWidth > 0 && renderer.remoteHeight > 0) {
        input.setRemoteSize(renderer.remoteWidth, renderer.remoteHeight);
    }
};

function renderLoop() {
    if (document.visibilityState === 'visible' && videoEl.readyState >= 2 && videoEl.videoWidth > 0) {
        renderer.drawVideoFrame(videoEl);
        if (renderer.remoteWidth > 0 && renderer.remoteHeight > 0) {
            input.setRemoteSize(renderer.remoteWidth, renderer.remoteHeight);
        }
    }
    requestAnimationFrame(renderLoop);
}
requestAnimationFrame(renderLoop);

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
    const width = view.getUint16(10, true);
    const height = view.getUint16(12, true);
    const payload = new Uint8Array(data, 14);

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
        console.log('[session]', JSON.parse(text));
    } catch (e) {
        console.warn('[session] invalid JSON:', e);
    }
}

transport.onStateChange = (state) => {
    switch (state) {
        case 'connecting':
            connectionLabel.textContent = readOnly ? 'Connecting (read-only)...' : 'Connecting...';
            overlay.classList.remove('hidden');
            break;
        case 'connected':
            connectionLabel.textContent = readOnly ? 'Connected (read-only)' : 'Connected';
            overlay.classList.add('hidden');
            sendClientReady();
            if (!readOnly) {
                input.releaseRemoteAll();
            }
            pollStatus();
            startStatusPolling();
            break;
        case 'disconnected':
            connectionLabel.textContent = 'Disconnected - reconnecting...';
            overlay.classList.remove('hidden');
            stopStatusPolling();
            break;
    }
};

function sendClientReady() {
    const buf = new ArrayBuffer(1);
    new DataView(buf).setUint8(0, CLI_CLIENT_READY);
    transport.send(buf);
}

function buildWsUrl() {
    const search = new URLSearchParams();
    if (readOnly) {
        search.set('readonly', '1');
    }
    const suffix = search.toString();
    return `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/ws${suffix ? `?${suffix}` : ''}`;
}

function updateStats() {
    statFps.textContent = `${renderer.fps} FPS`;
    statLatency.textContent = `q:${decoder.queueSize}`;
    if (renderer.remoteWidth > 0) {
        statResolution.textContent = `${renderer.remoteWidth}x${renderer.remoteHeight}`;
    }
    setTimeout(updateStats, 500);
}
updateStats();

function toggleFullscreen() {
    if (!document.fullscreenElement) {
        document.documentElement.requestFullscreen().catch(() => {});
    } else {
        document.exitFullscreen().catch(() => {});
    }
}

function renderSidebarState() {
    appShell.classList.toggle('sidebar-collapsed', sidebarCollapsed);
    btnSidebarCollapse.textContent = sidebarCollapsed ? 'Show' : 'Hide';
    btnSidebarCollapse.setAttribute('aria-expanded', String(!sidebarCollapsed));
    btnSidebarPeek.classList.toggle('hidden', !sidebarCollapsed);
    btnSidebarPeek.setAttribute('aria-expanded', String(!sidebarCollapsed));
}

function setSidebarCollapsed(nextCollapsed) {
    sidebarCollapsed = nextCollapsed;
    localStorage.setItem(SIDEBAR_STATE_KEY, sidebarCollapsed ? '1' : '0');
    renderSidebarState();
}

function renderSessionSwitcher() {
    const sessions = [
        { id: 'virtual', label: 'Virtual', port: SESSION_PORTS.virtual },
        { id: 'physical', label: 'Physical', port: SESSION_PORTS.physical },
    ];

    helperSession.textContent = `session:${currentSession}${readOnly ? ':ro' : ':rw'}`;
    sessionSwitcher.replaceChildren(
        ...sessions.map((session) => {
            const link = document.createElement('a');
            const params = new URLSearchParams(location.search);
            if (readOnly) {
                params.set('readonly', '1');
            } else {
                params.delete('readonly');
            }
            link.className = `session-button${session.id === currentSession ? ' active' : ''}`;
            link.href = `${location.protocol}//${location.hostname}:${session.port}/${params.toString() ? `?${params}` : ''}`;
            link.textContent = session.label;
            return link;
        }),
    );
    btnReadOnly.textContent = readOnly ? 'Writable' : 'Read-Only';
    btnReadOnly.classList.toggle('active', readOnly);
}

function setWarning(message) {
    if (!message) {
        warningBanner.textContent = '';
        warningBanner.classList.add('hidden');
        return;
    }
    warningBanner.textContent = message;
    warningBanner.classList.remove('hidden');
}

async function pollStatus() {
    try {
        const response = await fetch('/api/status', { cache: 'no-store' });
        if (!response.ok) {
            return;
        }
        const status = await response.json();
        renderStatus(status);
    } catch (_) {
        // ignore transient fetch failures
    }
}

function renderStatus(status) {
    serverStatus = status;
    bitrateSelect.value = String(status.bitrate);
    const lines = [
        `Target: ${status.session_name} ${status.display}`,
        `Resolution: ${status.width}x${status.height} @ ${status.fps} fps / ${(status.bitrate / 1_000_000).toFixed(0)} Mbps`,
        `Clients: ${status.total_clients} total / ${status.writable_clients} write / ${status.readonly_clients} read-only`,
    ];
    statusPanel.replaceChildren(
        ...lines.map((line) => {
            const item = document.createElement('div');
            item.textContent = line;
            return item;
        }),
    );

    if (status.multiple_writers) {
        setWarning('Multiple writable clients are connected to this session.');
    } else if (readOnly) {
        setWarning('Read-only mode is enabled. Input forwarding is disabled.');
    } else {
        setWarning('');
    }
}

function startStatusPolling() {
    stopStatusPolling();
    statusPollTimer = setInterval(pollStatus, 2500);
}

function stopStatusPolling() {
    if (statusPollTimer !== null) {
        clearInterval(statusPollTimer);
        statusPollTimer = null;
    }
}

function applyReadOnly(nextReadOnly) {
    readOnly = nextReadOnly;
    input.setReadOnly(readOnly);
    renderSessionSwitcher();
    const params = new URLSearchParams(location.search);
    if (readOnly) {
        params.set('readonly', '1');
    } else {
        params.delete('readonly');
    }
    const nextUrl = `${location.pathname}${params.toString() ? `?${params}` : ''}`;
    history.replaceState({}, '', nextUrl);
    transport.connect(buildWsUrl());
}

btnReconnect.addEventListener('click', () => {
    input.releaseCapture();
    transport.connect(buildWsUrl());
});

btnSidebarCollapse.addEventListener('click', () => {
    setSidebarCollapsed(!sidebarCollapsed);
});

btnSidebarPeek.addEventListener('click', () => {
    setSidebarCollapsed(false);
});

btnReadOnly.addEventListener('click', () => {
    transport.disconnect();
    applyReadOnly(!readOnly);
});

btnRestart.addEventListener('click', async () => {
    const bitrate = Number.parseInt(bitrateSelect.value, 10);
    connectionLabel.textContent = 'Restarting server...';
    overlay.classList.remove('hidden');
    try {
        await fetch('/api/control/restart', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ bitrate }),
        });
    } catch (_) {
        // the server may drop before the response completes
    }
    transport.disconnect();
    setTimeout(() => transport.connect(buildWsUrl()), 1200);
});

btnReleaseKeys.addEventListener('click', () => input.releaseRemoteKeys());
btnReleaseMouse.addEventListener('click', () => input.releaseRemoteMouse());
btnReleaseAll.addEventListener('click', () => input.releaseRemoteAll());
btnSendSuper.addEventListener('click', () => input.sendKeyTap(91));
btnPasteHostClipboard.addEventListener('click', async () => {
    if (!navigator.clipboard?.readText) {
        setWarning('Host clipboard read is not available in this browser context.');
        return;
    }
    try {
        const text = await navigator.clipboard.readText();
        if (!text) {
            setWarning('Host clipboard is empty.');
            return;
        }
        pasteText.value = text;
        input.pasteText(text);
        setWarning('Host clipboard sent to remote session.');
    } catch (err) {
        console.warn(err);
        setWarning('Host clipboard read failed. Use the text box if the browser blocks clipboard access.');
    }
});
btnSendText.addEventListener('click', () => input.pasteText(pasteText.value));
btnClearText.addEventListener('click', () => {
    pasteText.value = '';
    pasteText.focus();
});
btnPullClipboard.addEventListener('click', async () => {
    try {
        const response = await fetch('/api/clipboard/clipboard', { cache: 'no-store' });
        if (!response.ok) {
            throw new Error(`clipboard fetch failed: ${response.status}`);
        }
        const payload = await response.json();
        pasteText.value = payload.text ?? '';
        if (navigator.clipboard?.writeText) {
            await navigator.clipboard.writeText(pasteText.value);
            setWarning('Remote clipboard copied to host clipboard.');
            setTimeout(() => {
                if (serverStatus?.multiple_writers) {
                    setWarning('Multiple writable clients are connected to this session.');
                } else if (readOnly) {
                    setWarning('Read-only mode is enabled. Input forwarding is disabled.');
                } else {
                    setWarning('');
                }
            }, 1500);
        }
    } catch (err) {
        console.warn(err);
        setWarning('Remote clipboard pull failed.');
    }
});

window.addEventListener('keydown', (e) => {
    if (e.key === 'F11') {
        e.preventDefault();
        toggleFullscreen();
    }
});

canvas.addEventListener('dblclick', () => toggleFullscreen());
document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'hidden') {
        input.releaseCapture();
    }
});

sidebarCollapsed = localStorage.getItem(SIDEBAR_STATE_KEY) === '1';
renderSidebarState();
renderSessionSwitcher();
transport.connect(buildWsUrl());
