/**
 * WebSocket transport layer for the ddisplay binary protocol.
 */
export class Transport {
    /** @type {WebSocket|null} */
    #ws = null;
    /** @type {string} */
    #url = '';
    /** @type {'disconnected'|'connecting'|'connected'} */
    #state = 'disconnected';
    /** @type {number} */
    #reconnectDelay = 1000;
    /** @type {number|null} */
    #reconnectTimer = null;
    /** @type {boolean} */
    #intentionalClose = false;

    static RECONNECT_BASE = 1000;
    static RECONNECT_MAX = 30000;

    /**
     * Called with an ArrayBuffer for every binary message received.
     * @type {((data: ArrayBuffer) => void)|null}
     */
    onMessage = null;

    /**
     * Called whenever the connection state changes.
     * @type {((state: string) => void)|null}
     */
    onStateChange = null;

    /** Current connection state. */
    get state() {
        return this.#state;
    }

    /**
     * Open a WebSocket connection to the given URL.
     * @param {string} url  WebSocket URL (ws:// or wss://)
     */
    connect(url) {
        this.#url = url;
        this.#intentionalClose = false;
        this.#reconnectDelay = Transport.RECONNECT_BASE;
        this.#open();
    }

    /**
     * Send a binary message to the server.
     * @param {ArrayBuffer} buffer
     */
    send(buffer) {
        if (this.#ws && this.#ws.readyState === WebSocket.OPEN) {
            this.#ws.send(buffer);
        }
    }

    /** Gracefully close the connection without auto-reconnect. */
    disconnect() {
        this.#intentionalClose = true;
        this.#clearReconnect();
        if (this.#ws) {
            this.#ws.close();
            this.#ws = null;
        }
        this.#setState('disconnected');
    }

    // -- internals --

    #open() {
        this.#clearReconnect();

        if (this.#ws) {
            this.#ws.onopen = null;
            this.#ws.onclose = null;
            this.#ws.onerror = null;
            this.#ws.onmessage = null;
            try { this.#ws.close(); } catch (_) { /* ignore */ }
            this.#ws = null;
        }

        this.#setState('connecting');

        const ws = new WebSocket(this.#url);
        ws.binaryType = 'arraybuffer';

        ws.onopen = () => {
            this.#reconnectDelay = Transport.RECONNECT_BASE;
            this.#setState('connected');
        };

        ws.onclose = () => {
            this.#ws = null;
            this.#setState('disconnected');
            if (!this.#intentionalClose) {
                this.#scheduleReconnect();
            }
        };

        ws.onerror = () => {
            // onclose will fire after this, triggering reconnect.
        };

        ws.onmessage = (event) => {
            if (event.data instanceof ArrayBuffer && this.onMessage) {
                this.onMessage(event.data);
            }
        };

        this.#ws = ws;
    }

    /**
     * @param {'disconnected'|'connecting'|'connected'} s
     */
    #setState(s) {
        if (this.#state === s) return;
        this.#state = s;
        if (this.onStateChange) {
            this.onStateChange(s);
        }
    }

    #scheduleReconnect() {
        this.#clearReconnect();
        this.#reconnectTimer = setTimeout(() => {
            this.#reconnectTimer = null;
            this.#reconnectDelay = Math.min(this.#reconnectDelay * 2, Transport.RECONNECT_MAX);
            this.#open();
        }, this.#reconnectDelay);
    }

    #clearReconnect() {
        if (this.#reconnectTimer !== null) {
            clearTimeout(this.#reconnectTimer);
            this.#reconnectTimer = null;
        }
    }
}
