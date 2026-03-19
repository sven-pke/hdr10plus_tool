# hdr10plus_tool

CLI utility to work with HDR10+ dynamic metadata in **AV1** video bitstreams.

Supports both raw AV1 OBU streams and IVF-containerized AV1 (e.g. output from `aomenc`/`libaom`).

&nbsp;

## Building

### Toolchain

Minimum Rust version: **1.85.0**

### Dependencies

On Linux, [fontconfig](https://github.com/yeslogic/fontconfig-rs#dependencies) is required.
Alternatively bypass system fonts with `--no-default-features --features internal-font`.

```console
cargo build --release
```

&nbsp;

## Global options

These apply to all commands:

| Flag | Description |
|------|-------------|
| `--verify` | Check whether the input file contains HDR10+ metadata, then exit. |
| `--skip-validation` | Skip HDR10+ profile conformity validation. Invalid metadata is accepted as-is. |

&nbsp;

## Commands

* ### **extract**

    Extracts HDR10+ metadata from an AV1 bitstream and writes it to a JSON file.
    Also calculates scene information for compatibility with Samsung tools.

    If no output file is specified (`-o`), the command only verifies the presence of metadata and exits early (same as `--verify`).

    **Supported input formats:**
    - Raw AV1 OBU stream (`.obu`, `.av1`)
    - IVF container (`.ivf`)

    **Flags:**
    - `-l`, `--limit` — Stop after processing N OBUs.

    **Examples:**
    ```console
    hdr10plus_tool extract video.ivf -o metadata.json

    hdr10plus_tool extract video.obu -o metadata.json

    # Verify presence only (no output file)
    hdr10plus_tool --verify extract video.ivf

    # Skip validation
    hdr10plus_tool --skip-validation extract video.ivf -o metadata.json

    # Pipe from ffmpeg
    ffmpeg -i input.mkv -map 0:v:0 -c copy -f ivf - | hdr10plus_tool extract -o metadata.json -
    ```

&nbsp;

* ### **inject**

    Interleaves HDR10+ metadata `OBU_METADATA` units into an AV1 bitstream, one per temporal unit.
    `--verify` has no effect with this command.

    The metadata OBU is placed immediately after the `OBU_TEMPORAL_DELIMITER` of each temporal unit.
    Any existing HDR10+ OBUs in the input are replaced.

    **Mismatch handling:**
    - If the JSON has **more** entries than video frames, excess metadata is silently ignored with a warning.
    - If the JSON has **fewer** entries than video frames, the last metadata entry is duplicated for the remaining frames with a warning.

    **Examples:**
    ```console
    hdr10plus_tool inject -i video.ivf -j metadata.json -o injected_output.ivf

    hdr10plus_tool inject -i video.obu -j metadata.json -o injected_output.av1
    ```

&nbsp;

* ### **remove**

    Removes all HDR10+ `OBU_METADATA` units from an AV1 bitstream.
    `--verify` has no effect with this command.

    **Examples:**
    ```console
    hdr10plus_tool remove video.ivf -o hdr10plus_removed_output.ivf

    hdr10plus_tool remove video.obu -o hdr10plus_removed_output.av1
    ```

&nbsp;

* ### **plot**

    Plots HDR10+ brightness metadata from a JSON file as a PNG graph.

    **Flags:**
    - `-t`, `--title` — Title at the top of the plot.
    - `-p`, `--peak-source` — How to extract peak brightness. One of: `histogram` (default), `histogram99`, `max-scl`, `max-scl-luminance`.
    - `-s`, `--start` — Frame range start.
    - `-e`, `--end` — Frame range end (inclusive).

    **Example:**
    ```console
    hdr10plus_tool plot metadata.json -t "HDR10+ plot" -o hdr10plus_plot.png
    ```

&nbsp;

* ### **editor**

    Adds or removes frames from a HDR10+ JSON metadata file.

    **`edits.json` format:**
    ```json5
    {
        // List of frames or frame ranges to remove (inclusive)
        // Frames are removed before the duplicate passes
        "remove": [
            "0-39"
        ],

        // List of duplicate operations
        "duplicate": [
            {
                // Frame to use as metadata source
                "source": 0,
                // Index at which the duplicated frames are added (inclusive)
                "offset": 40,
                // Number of frames to duplicate
                "length": 5
            }
        ]
    }
    ```

    **Example:**
    ```console
    hdr10plus_tool editor metadata.json -j edits.json -o metadata_modified.json
    ```

&nbsp;

---

## Architecture and implementation details

This section documents the complete AV1 migration and internal design.
The original tool worked with **HEVC** bitstreams using Annex B NAL units and SEI messages.
The tool was fully rewritten to target **AV1** bitstreams with OBU_METADATA units.

### Why AV1 is simpler than HEVC for HDR10+ injection

In HEVC, frames can be stored in **decode order**, which differs from **display order** due to B-frames.
HDR10+ metadata must be paired with the correct display frame, so HEVC required a complex two-pass reorder algorithm.

AV1 temporal units are always in **display order** — there is no B-frame reordering.
Injection is therefore a simple single-pass stream operation: one metadata OBU per temporal unit.

---

### AV1 bitstream concepts

#### OBU (Open Bitstream Unit)

The fundamental unit of an AV1 bitstream. Every OBU has:

```
┌──────────────────────────────────────────────────┐
│  Header byte                                     │
│    bit 7   : forbidden_zero_bit = 0              │
│    bits 6-3: obu_type (4 bits)                   │
│    bit 2   : obu_extension_flag                  │
│    bit 1   : obu_has_size_field                  │
│    bit 0   : reserved = 0                        │
├──────────────────────────────────────────────────┤
│  Optional extension byte (if extension_flag=1)   │
│    bits 7-5: temporal_id                         │
│    bits 4-3: spatial_id                          │
│    bits 2-0: reserved                            │
├──────────────────────────────────────────────────┤
│  Payload size (LEB128, only if size_field=1)     │
├──────────────────────────────────────────────────┤
│  Payload                                         │
└──────────────────────────────────────────────────┘
```

**This tool only supports the Low Overhead Bitstream Format**, where every OBU carries `obu_has_size_field = 1`. This is the format used by IVF containers and most AV1 encoders.

#### OBU types (AV1 spec Table 5)

| Constant | Value | Meaning |
|----------|-------|---------|
| `OBU_SEQUENCE_HEADER` | 1 | Codec configuration (color space, dimensions, …) |
| `OBU_TEMPORAL_DELIMITER` | 2 | Marks the start of a new temporal unit |
| `OBU_FRAME_HEADER` | 3 | Frame header (separate from tile data) |
| `OBU_METADATA` | 5 | Metadata (HDR, film grain, …) |
| `OBU_FRAME` | 6 | Combined frame header + tile data |
| `OBU_REDUNDANT_FRAME_HEADER` | 7 | Redundant copy of a frame header |

#### Temporal unit (TU)

A temporal unit is the set of OBUs that together decode one video frame for display.
TUs are delimited by `OBU_TEMPORAL_DELIMITER` in raw streams.
In IVF containers, each IVF frame corresponds to exactly one temporal unit.

Typical structure of a temporal unit:
```
OBU_TEMPORAL_DELIMITER
  [HDR10+ OBU_METADATA]      ← injected here
  OBU_SEQUENCE_HEADER        ← present in first TU, sometimes repeated
  OBU_FRAME (or OBU_FRAME_HEADER + OBU_TILE_GROUP)
```

#### LEB128 encoding

OBU payload sizes and some OBU-internal fields (e.g. `metadata_type`) are encoded as **LEB128** (unsigned, little-endian base-128 variable length).

```
value 4 → single byte 0x04
value 128 → two bytes [0x80, 0x01]
```

---

### HDR10+ in AV1

HDR10+ is carried as an `OBU_METADATA` with `metadata_type = METADATA_TYPE_ITUT_T35 = 4`.

#### OBU_METADATA payload layout for HDR10+

```
metadata_type     LEB128     = 0x04
country_code      u8         = 0xB5   (USA)
terminal_provider_code  u16 BE = 0x003C
terminal_provider_oriented_code u16 BE = 0x0001
application_identifier u8   = 4
application_version    u8   = 1
<HDR10+ bitstream>
```

#### OBU header byte for HDR10+ metadata

```
(obu_type=5) << 3 | obu_has_size_field=1 = 0x2A
```

The complete encoded OBU is:
```
0x2A  <LEB128(payload_len)>  0x04  0xB5  0x00 0x3C  0x00 0x01  0x04  0x01  <HDR10+ bits>
```

---

### IVF container format

IVF is a simple container for raw AV1 bitstreams produced by encoders such as `aomenc`.

#### File header (32 bytes)

```
bytes  0– 3:  magic "DKIF"
bytes  4– 5:  version (u16 LE)
bytes  6– 7:  header_size = 32 (u16 LE)
bytes  8–11:  codec FourCC (e.g. "AV01")
bytes 12–13:  width (u16 LE)
bytes 14–15:  height (u16 LE)
bytes 16–19:  timebase denominator (u32 LE)
bytes 20–23:  timebase numerator (u32 LE)
bytes 24–27:  frame count (u32 LE)
bytes 28–31:  reserved
```

#### Frame header (12 bytes, repeated per temporal unit)

```
bytes 0– 3:  frame_size (u32 LE)  — size of the OBU data that follows
bytes 4–11:  timestamp (u64 LE)   — in stream timebase units
```

Each IVF frame contains exactly the OBUs of one temporal unit (Low Overhead Bitstream Format).

**Detection:** The tool peeks at the first 4 bytes using `BufRead::fill_buf()` to check for the `DKIF` magic without consuming them. If present, the 32-byte file header is consumed and IVF mode is activated. Otherwise, raw OBU stream mode is used.

---

### Stateful AV1 parser (`src/core/av1_parser.rs`)

#### `Obu::read_from<R: Read>`

Reads one complete OBU from a byte stream:

1. Read 1-byte header.
2. If `extension_flag`, read 1 more byte (temporal_id, spatial_id).
3. Read LEB128 payload size.
4. Read payload bytes.
5. Store header + size_bytes + payload as `raw_bytes` for lossless pass-through.

Returns `None` on clean EOF, error on malformed data.

#### `Av1NaluParser`

A stateful parser that tracks:
- `reduced_still_picture_header` — extracted from the sequence header (bit 5 of the SH payload). Affects how frame headers are interpreted.
- `temporal_unit_count` — incremented on each `OBU_TEMPORAL_DELIMITER`.
- `temporal_units: Vec<TemporalUnitInfo>` — per-TU display info:
  - `index` — zero-based TU index.
  - `is_displayed` — true if this TU produces a visible output frame.
  - `is_show_existing` — true if this TU reuses a previously decoded frame.

**Frame display detection** (from frame header payload):
1. If `reduced_still_picture_header`: always displayed (show_existing=0, show_frame=1).
2. Read `show_existing_frame` (1 bit). If 1 → displayed.
3. Skip `frame_type` (2 bits).
4. Read `show_frame` (1 bit). If 1 → displayed.
5. Otherwise read `showable_frame` (1 bit).

This minimal parsing (5 bits total) is sufficient to determine display status without implementing full AV1 decoding.

---

### Injection logic (`src/commands/inject.rs`)

#### Container detection

```
open input file as BufReader
  → try_read_ivf_file_header()
       DKIF found → write IVF file header → inject_ivf()
       not found  →                          inject_raw()
```

#### IVF injection (`inject_ivf`)

```
for each IVF frame:
  read 12-byte frame header (frame_size, timestamp)
  read frame_size bytes
  parse OBUs from frame data
  build_output_frame(obus, encoded_hdr10plus)
  write updated IVF frame header (new frame_size)
  write output frame bytes
```

#### Raw OBU injection (`inject_raw`)

State machine that buffers one temporal unit at a time:

```
current_td = None
pending = []

for each OBU (or EOF):
  if is_temporal_delimiter or EOF, and current_td is set:
    flush TU:
      write current_td.raw_bytes
      write encoded_hdr10plus
      write pending OBUs (skip existing HDR10+)
    tu_index += 1

  if OBU is temporal_delimiter:
    current_td = OBU
    pending.clear()
  elif current_td is set:
    pending.push(OBU)
  else:
    write OBU directly (pre-stream OBUs before first TD)
```

#### `build_output_frame`

Constructs the output byte buffer for one temporal unit:

1. Find position of `OBU_TEMPORAL_DELIMITER` → `insert_after_td = pos + 1` (or 0 if no TD).
2. Iterate OBUs. When index reaches `insert_after_td`, emit `encoded` HDR10+ bytes first.
3. Skip (drop) any existing HDR10+ OBU (`is_hdr10plus_obu`).
4. Copy all other OBUs as-is via `raw_bytes`.
5. If no insertion happened (no OBUs at all), append `encoded` at the end.

---

### Encoding (`hdr10plus/src/av1/mod.rs`)

`encode_hdr10plus_obu(metadata, validate)`:

1. Encode with `Hdr10PlusMetadataEncOpts { with_country_code: true }` → T.35 payload starting at `0xB5`.
2. Prepend `metadata_type = 4` as LEB128 → OBU payload.
3. Encode payload length as LEB128.
4. Prepend header byte `0x2A`.

`encode_av1_from_json(json_metadata, validate)` (requires `json` feature):
Converts `Hdr10PlusJsonMetadata` → `Hdr10PlusMetadata` → calls `encode_hdr10plus_obu`.

---

### Extraction logic (`src/commands/extract.rs`)

1. Detect IVF vs raw.
2. Parse each OBU. For every `OBU_METADATA`, call `extract_hdr10plus_t35_bytes`.
3. `extract_hdr10plus_t35_bytes` verifies: `metadata_type=4`, `country_code=0xB5`, `provider_code=0x003C`, `oriented_code=0x0001`, `app_id=4`, `app_version=1`. Returns T.35 bytes starting at country code.
4. Collected T.35 bytes are parsed to `Hdr10PlusMetadata` structs.
5. JSON is generated via `generate_json(list, TOOL_NAME, TOOL_VERSION)`.

---

### Remove logic (`src/commands/remove.rs`)

For each OBU (or IVF frame):
- If `is_hdr10plus_obu` → skip.
- Otherwise → write `raw_bytes` unchanged.

For IVF: frame headers are rewritten with the updated `frame_size` after dropping HDR10+ OBUs.

---

### Known issue: MediaInfo double HDR10+ display after mkvmerge

When an injected IVF file is muxed into MKV using `mkvmerge`, MediaInfo may report HDR10+ twice:
```
SMPTE ST 2094 App 4 / SMPTE ST 2094 App 4
```

**Root cause:** mkvmerge copies all OBUs that appear before the first `OBU_FRAME`/`OBU_TILE_GROUP` in the first IVF frame into the `AV1CodecConfigurationRecord` (codec private data). This includes our injected HDR10+ OBU. MediaInfo then finds it in both the codec private record and the bitstream.

**This is a mkvmerge behavior, not a bug in hdr10plus_tool.** The IVF bitstream itself is correct. On tested players, playback works correctly.

**Do not** attempt to fix this by moving the HDR10+ OBU to after the `OBU_FRAME` — this breaks playback.

---

### File structure

```
hdr10plus/
  src/
    av1/mod.rs          — HDR10+ OBU encoding + LEB128 utilities
    lib.rs              — exposes pub mod av1 (unconditional, no feature gate)

src/
  core/
    mod.rs              — ParserError, initialize_progress_bar, constants
    av1_parser.rs       — Obu, Av1NaluParser, IVF support, HDR10+ detection
  commands/
    mod.rs              — CLI arg structs (ExtractArgs, InjectArgs, RemoveArgs, …)
    extract.rs          — Extractor: IVF + raw AV1 HDR10+ extraction
    inject.rs           — Injector: IVF + raw AV1 HDR10+ injection
    remove.rs           — Remover: IVF + raw AV1 HDR10+ removal
    plot.rs             — Plotter: JSON → PNG brightness graph
    editor.rs           — Editor: JSON frame add/remove
  main.rs               — CLI entry point (clap)
```

---

### Sample / test workflow

```console
# Encode AV1 to IVF (example with aomenc)
aomenc input.y4m --ivf -o video.ivf

# Extract existing HDR10+ (if present)
hdr10plus_tool extract video.ivf -o metadata.json

# Inject HDR10+ from JSON
hdr10plus_tool inject -i video.ivf -j metadata.json -o injected_output.ivf

# Verify injection
hdr10plus_tool --verify extract injected_output.ivf

# Mux to MKV (note: MediaInfo may show HDR10+ twice — see known issue above)
mkvmerge -o output.mkv injected_output.ivf

# Remove HDR10+ (for testing)
hdr10plus_tool remove injected_output.ivf -o clean_output.ivf
```
