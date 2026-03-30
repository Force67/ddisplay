/**
 * Lightweight fMP4 (fragmented MP4) muxer for H.264 Annex B streams.
 *
 * Converts raw H.264 NAL units into ISO BMFF (fMP4) segments that can
 * be fed into a MediaSource SourceBuffer. Produces:
 *   1. An initialization segment (ftyp + moov) on first keyframe
 *   2. Media segments (moof + mdat) for each frame
 */

/**
 * Parse H.264 Annex B data into individual NAL units (without start codes).
 * @param {Uint8Array} data
 * @returns {Uint8Array[]}
 */
export function parseNALUs(data) {
    const nalus = [];
    let i = 0;
    while (i < data.length - 3) {
        // Find start code
        if (data[i] === 0 && data[i + 1] === 0) {
            let scLen;
            if (data[i + 2] === 1) {
                scLen = 3;
            } else if (data[i + 2] === 0 && i + 3 < data.length && data[i + 3] === 1) {
                scLen = 4;
            } else {
                i++;
                continue;
            }
            // Find next start code to determine NAL end
            const nalStart = i + scLen;
            let nalEnd = data.length;
            for (let j = nalStart + 1; j < data.length - 3; j++) {
                if (data[j] === 0 && data[j + 1] === 0 &&
                    (data[j + 2] === 1 || (data[j + 2] === 0 && j + 3 < data.length && data[j + 3] === 1))) {
                    nalEnd = j;
                    break;
                }
            }
            nalus.push(data.subarray(nalStart, nalEnd));
            i = nalEnd;
        } else {
            i++;
        }
    }
    return nalus;
}

/**
 * Create a 4-byte big-endian length-prefixed NAL unit (AVCC format).
 * @param {Uint8Array} nalu
 * @returns {Uint8Array}
 */
function lengthPrefixNALU(nalu) {
    const out = new Uint8Array(4 + nalu.length);
    const dv = new DataView(out.buffer);
    dv.setUint32(0, nalu.length, false); // big-endian
    out.set(nalu, 4);
    return out;
}

/**
 * Write a 32-bit big-endian value into a Uint8Array at offset.
 */
function writeU32(arr, offset, value) {
    arr[offset] = (value >> 24) & 0xff;
    arr[offset + 1] = (value >> 16) & 0xff;
    arr[offset + 2] = (value >> 8) & 0xff;
    arr[offset + 3] = value & 0xff;
}

/**
 * Create an MP4 box.
 * @param {string} type  4-char box type
 * @param  {...Uint8Array} payloads
 * @returns {Uint8Array}
 */
function box(type, ...payloads) {
    let size = 8;
    for (const p of payloads) size += p.length;
    const out = new Uint8Array(size);
    writeU32(out, 0, size);
    out[4] = type.charCodeAt(0);
    out[5] = type.charCodeAt(1);
    out[6] = type.charCodeAt(2);
    out[7] = type.charCodeAt(3);
    let offset = 8;
    for (const p of payloads) {
        out.set(p, offset);
        offset += p.length;
    }
    return out;
}

/**
 * Create the ftyp box.
 */
function ftyp() {
    return box('ftyp',
        new Uint8Array([
            0x69, 0x73, 0x6f, 0x6d, // major_brand: isom
            0x00, 0x00, 0x00, 0x01, // minor_version: 1
            0x69, 0x73, 0x6f, 0x6d, // isom
            0x61, 0x76, 0x63, 0x31, // avc1
        ])
    );
}

/**
 * Build the moov box (initialization segment) from SPS and PPS NALUs.
 * @param {Uint8Array} sps
 * @param {Uint8Array} pps
 * @param {number} width
 * @param {number} height
 * @returns {Uint8Array}
 */
function moov(sps, pps, width, height) {
    const timescale = 90000;

    // avcC box content (AVCDecoderConfigurationRecord)
    const avcC_data = new Uint8Array([
        0x01,           // configurationVersion
        sps[1],         // profile_idc
        sps[2],         // constraint_set_flags
        sps[3],         // level_idc
        0xff,           // lengthSizeMinusOne = 3 (4-byte lengths)
        0xe1,           // numOfSequenceParameterSets = 1
        (sps.length >> 8) & 0xff, sps.length & 0xff,
        ...sps,
        0x01,           // numOfPictureParameterSets = 1
        (pps.length >> 8) & 0xff, pps.length & 0xff,
        ...pps,
    ]);

    const avcC = box('avcC', avcC_data);

    const stbl = box('stbl',
        box('stsd', new Uint8Array([
            0x00, 0x00, 0x00, 0x00, // version + flags
            0x00, 0x00, 0x00, 0x01, // entry_count = 1
        ]),
            // avc1 visual sample entry
            createAVC1Entry(width, height, avcC)
        ),
        box('stts', new Uint8Array([0, 0, 0, 0, 0, 0, 0, 0])), // empty
        box('stsc', new Uint8Array([0, 0, 0, 0, 0, 0, 0, 0])),
        box('stsz', new Uint8Array([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])),
        box('stco', new Uint8Array([0, 0, 0, 0, 0, 0, 0, 0])),
    );

    const dinf = box('dinf',
        box('dref', new Uint8Array([
            0x00, 0x00, 0x00, 0x00, // version + flags
            0x00, 0x00, 0x00, 0x01, // entry_count = 1
        ]),
            box('url ', new Uint8Array([0x00, 0x00, 0x00, 0x01])) // self-contained
        )
    );

    const minf = box('minf',
        box('vmhd', new Uint8Array([
            0x00, 0x00, 0x00, 0x01, // version=0, flags=1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00 // graphicsmode, opcolor
        ])),
        dinf,
        stbl,
    );

    const mdia = box('mdia',
        box('mdhd', new Uint8Array([
            0x00, 0x00, 0x00, 0x00, // version + flags
            0x00, 0x00, 0x00, 0x00, // creation_time
            0x00, 0x00, 0x00, 0x00, // modification_time
            (timescale >> 24) & 0xff, (timescale >> 16) & 0xff,
            (timescale >> 8) & 0xff, timescale & 0xff,
            0x00, 0x00, 0x00, 0x00, // duration
            0x55, 0xc4,             // language: und
            0x00, 0x00,             // pre_defined
        ])),
        box('hdlr', new Uint8Array([
            0x00, 0x00, 0x00, 0x00, // version + flags
            0x00, 0x00, 0x00, 0x00, // pre_defined
            0x76, 0x69, 0x64, 0x65, // handler_type: vide
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, // name (null terminated)
        ])),
        minf,
    );

    const trak = box('trak',
        box('tkhd', createTKHD(width, height)),
        mdia,
    );

    const mvex = box('mvex',
        box('trex', new Uint8Array([
            0x00, 0x00, 0x00, 0x00, // version + flags
            0x00, 0x00, 0x00, 0x01, // track_ID
            0x00, 0x00, 0x00, 0x01, // default_sample_description_index
            0x00, 0x00, 0x00, 0x00, // default_sample_duration
            0x00, 0x00, 0x00, 0x00, // default_sample_size
            0x00, 0x01, 0x00, 0x00, // default_sample_flags (non-sync)
        ])),
    );

    const mvhd_data = new Uint8Array([
        0x00, 0x00, 0x00, 0x00, // version + flags
        0x00, 0x00, 0x00, 0x00, // creation_time
        0x00, 0x00, 0x00, 0x00, // modification_time
        (timescale >> 24) & 0xff, (timescale >> 16) & 0xff,
        (timescale >> 8) & 0xff, timescale & 0xff,
        0x00, 0x00, 0x00, 0x00, // duration
        0x00, 0x01, 0x00, 0x00, // rate: 1.0
        0x01, 0x00,             // volume: 1.0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
        // unity matrix
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
        // pre_defined[6]
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x02, // next_track_ID
    ]);

    return box('moov', box('mvhd', mvhd_data), trak, mvex);
}

function createTKHD(width, height) {
    const d = new Uint8Array(84);
    d[0] = 0; // version
    d[1] = 0; d[2] = 0; d[3] = 0x03; // flags: enabled + in_movie
    // track_ID = 1 at offset 12
    d[15] = 1;
    // width at offset 76 (as 16.16 fixed point)
    writeU32(d, 76, width << 16);
    // height at offset 80
    writeU32(d, 80, height << 16);
    // unity matrix at offset 36
    writeU32(d, 36, 0x00010000);
    writeU32(d, 52, 0x00010000);
    writeU32(d, 68, 0x40000000);
    return d;
}

function createAVC1Entry(width, height, avcC) {
    // avc1 box is a special "visual sample entry"
    const header = new Uint8Array(78);
    // reserved (6 bytes) + data_reference_index (2 bytes)
    header[6] = 0; header[7] = 1; // data_ref_idx = 1
    // width at offset 24
    header[24] = (width >> 8) & 0xff;
    header[25] = width & 0xff;
    // height at offset 26
    header[26] = (height >> 8) & 0xff;
    header[27] = height & 0xff;
    // horiz resolution 72 dpi at offset 28 (0x00480000)
    header[28] = 0x00; header[29] = 0x48; header[30] = 0x00; header[31] = 0x00;
    // vert resolution 72 dpi at offset 32
    header[32] = 0x00; header[33] = 0x48; header[34] = 0x00; header[35] = 0x00;
    // frame_count = 1 at offset 40
    header[40] = 0; header[41] = 1;
    // depth = 0x0018 at offset 74
    header[74] = 0x00; header[75] = 0x18;
    // pre_defined = -1 at offset 76
    header[76] = 0xff; header[77] = 0xff;

    // Build the avc1 box manually (it's not a regular box - has sample entry header)
    const totalSize = 8 + header.length + avcC.length;
    const out = new Uint8Array(totalSize);
    writeU32(out, 0, totalSize);
    out[4] = 0x61; out[5] = 0x76; out[6] = 0x63; out[7] = 0x31; // 'avc1'
    out.set(header, 8);
    out.set(avcC, 8 + header.length);
    return out;
}

/**
 * Create a media segment (moof + mdat) for a single frame.
 * @param {Uint8Array[]} nalus  NAL units (excluding SPS/PPS)
 * @param {number} sequenceNumber  Fragment sequence number (1-based)
 * @param {number} duration  Sample duration in timescale units
 * @param {boolean} isKeyframe
 * @param {number} baseDecodeTime  Decode time in timescale units
 * @returns {Uint8Array}
 */
export function createMediaSegment(nalus, sequenceNumber, duration, isKeyframe, baseDecodeTime) {
    // Build mdat content: length-prefixed NALUs
    let mdatPayloadSize = 0;
    const prefixed = nalus.map(n => {
        const p = lengthPrefixNALU(n);
        mdatPayloadSize += p.length;
        return p;
    });

    // trun (track fragment run) - one sample
    const sampleFlags = isKeyframe ? 0x02000000 : 0x01010000;
    const trun_data = new Uint8Array(24);
    // version=0, flags=0x000b01 (data-offset + duration + size + flags present)
    trun_data[0] = 0; trun_data[1] = 0x00; trun_data[2] = 0x0b; trun_data[3] = 0x01;
    writeU32(trun_data, 4, 1); // sample_count = 1
    // data_offset: filled later
    writeU32(trun_data, 8, 0); // placeholder
    writeU32(trun_data, 12, duration);
    writeU32(trun_data, 16, mdatPayloadSize);
    writeU32(trun_data, 20, sampleFlags);

    const tfdt = box('tfdt', new Uint8Array([
        0x01, 0x00, 0x00, 0x00, // version=1, flags=0
        // baseMediaDecodeTime as u64
        ...u64Bytes(baseDecodeTime),
    ]));

    const tfhd_data = new Uint8Array(8);
    // version=0, flags=0x020000 (default-base-is-moof)
    tfhd_data[0] = 0; tfhd_data[1] = 0x02; tfhd_data[2] = 0x00; tfhd_data[3] = 0x00;
    writeU32(tfhd_data, 4, 1); // track_ID = 1

    const trun_box = box('trun', trun_data);
    const traf = box('traf', box('tfhd', tfhd_data), tfdt, trun_box);

    const mfhd_data = new Uint8Array(8);
    writeU32(mfhd_data, 4, sequenceNumber);
    const moof = box('moof', box('mfhd', mfhd_data), traf);

    // Fix data_offset in trun: it points from the start of moof to the start of mdat payload
    const mdatHeaderSize = 8;
    const dataOffset = moof.length + mdatHeaderSize;
    // Find trun data_offset position (byte 8 of trun_data, which is inside the trun box)
    // trun is inside traf, which is inside moof. We need to find the absolute position.
    // Simpler: just rebuild with correct offset.
    const trunOffset = findBoxOffset(moof, 'trun');
    if (trunOffset >= 0) {
        writeU32(moof, trunOffset + 8 + 8, dataOffset); // +8 for box header, +8 for version+flags+sample_count
    }

    // Build mdat
    const mdat = new Uint8Array(8 + mdatPayloadSize);
    writeU32(mdat, 0, mdat.length);
    mdat[4] = 0x6d; mdat[5] = 0x64; mdat[6] = 0x61; mdat[7] = 0x74; // 'mdat'
    let off = 8;
    for (const p of prefixed) {
        mdat.set(p, off);
        off += p.length;
    }

    // Concatenate moof + mdat
    const seg = new Uint8Array(moof.length + mdat.length);
    seg.set(moof, 0);
    seg.set(mdat, moof.length);
    return seg;
}

/**
 * Create the initialization segment from SPS/PPS NALUs.
 * @param {Uint8Array} sps
 * @param {Uint8Array} pps
 * @param {number} width
 * @param {number} height
 * @returns {Uint8Array}
 */
export function createInitSegment(sps, pps, width, height) {
    const ftypBox = ftyp();
    const moovBox = moov(sps, pps, width, height);
    const init = new Uint8Array(ftypBox.length + moovBox.length);
    init.set(ftypBox, 0);
    init.set(moovBox, ftypBox.length);
    return init;
}

/**
 * Find the byte offset of a named box within a parent box's data.
 */
function findBoxOffset(data, name) {
    let i = 8; // skip parent box header
    while (i < data.length - 8) {
        const size = (data[i] << 24) | (data[i + 1] << 16) | (data[i + 2] << 8) | data[i + 3];
        const n = String.fromCharCode(data[i + 4], data[i + 5], data[i + 6], data[i + 7]);
        if (n === name) return i;
        // Recurse into container boxes
        if (['moof', 'traf', 'mdia', 'minf', 'stbl', 'trak', 'moov'].includes(n)) {
            const inner = findBoxOffsetInner(data, i + 8, i + size, name);
            if (inner >= 0) return inner;
        }
        if (size < 8) break;
        i += size;
    }
    return -1;
}

function findBoxOffsetInner(data, start, end, name) {
    let i = start;
    while (i < end - 8) {
        const size = (data[i] << 24) | (data[i + 1] << 16) | (data[i + 2] << 8) | data[i + 3];
        const n = String.fromCharCode(data[i + 4], data[i + 5], data[i + 6], data[i + 7]);
        if (n === name) return i;
        if (['traf', 'mdia', 'minf', 'stbl'].includes(n)) {
            const inner = findBoxOffsetInner(data, i + 8, i + size, name);
            if (inner >= 0) return inner;
        }
        if (size < 8) break;
        i += size;
    }
    return -1;
}

function u64Bytes(value) {
    const hi = Math.floor(value / 0x100000000);
    const lo = value >>> 0;
    return new Uint8Array([
        (hi >> 24) & 0xff, (hi >> 16) & 0xff, (hi >> 8) & 0xff, hi & 0xff,
        (lo >> 24) & 0xff, (lo >> 16) & 0xff, (lo >> 8) & 0xff, lo & 0xff,
    ]);
}
