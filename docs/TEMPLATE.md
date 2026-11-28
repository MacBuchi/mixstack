# Mix templates — `mixstack.mix/1`

A template is a mix that is not tied to one recording: per-track faders,
pan, polarity, mute/solo/in-mix, EQ and the master settings. It is written
by the app, the `mixstack` CLI and the MCP server, and read by them and by
other tools (the band player). This page is the contract.

Implementation: `engine/src/template.rs`. Tests: `template_tests` in
`engine/tests/engine_tests.rs`.

## Document

```json
{
  "schema": "mixstack.mix/1",
  "name": "Band-Mix",
  "created": "2026-11-25T21:10:00Z",
  "source": { "take": "UFX55_00", "snapshot": "A" },
  "tracks": [
    { "name": "Guitar_O S L", "index": 33, "gain_db": -7.1, "pan": -1.0,
      "polarity_invert": false, "muted": false, "solo": false, "in_mix": true,
      "eq": { "hpf_enabled": true, "hpf_freq": 80.0, "hpf_slope": "Db12",
              "low":  { "enabled": false, "freq": 120.0,  "gain_db": 0.0, "q": 0.707 },
              "mid":  { "enabled": true,  "freq": 2500.0, "gain_db": -2.0, "q": 1.0 },
              "high": { "enabled": false, "freq": 8000.0, "gain_db": 0.0, "q": 0.707 } },
      "tags": ["gitarre"] }
  ],
  "settings": { "loudness": { "LufsIntegrated": -16.0 }, "limiter_enabled": true,
                "ceiling_dbtp": -1.0, "mastering": null }
}
```

| Field | Meaning |
|---|---|
| `schema` | **Required.** `mixstack.mix/<major>`. A reader refuses a major it does not know. |
| `name`, `created`, `source` | Informational. `created` is RFC 3339, set by the writer. Never used for mapping. |
| `tracks[]` | Same field names as a session's `TrackParams`. `index` is the 1-based interleave index in the take the template was made from (`0` = unknown). `gain_db` −60…+6, `pan` −1…+1. |
| `tracks[].tags` | Free roles ("vocals", "monitor"). Set only by a human, a configuration or a later detector. **The engine never interprets them**; they are carried through. |
| `settings` | The mix-level part of the render settings: `loudness` (`"None"`, `{"PeakDbfs": -1.0}`, `{"LufsIntegrated": -16.0}`), `limiter_enabled`, `ceiling_dbtp`, `mastering`. |

**Not in a template:** trim, fades, output format, dither. Those belong to one
render and are given per call (CLI flags, MCP arguments, the app's export
dialog).

Unknown fields are ignored when reading; missing fields take their defaults
(a track defaults to unity, centre, in the mix, EQ bypassed; settings default
to −1 dBFS peak normalisation with the −1 dBTP limiter on). Wherever a
template is accepted, a stored session (`*.durecmix.json`) is accepted too and
converted on the way in.

## Applying a template to a recording

Tracks are mapped in three rounds, each only over what the earlier rounds
left unclaimed; a template track is used at most once:

1. **exact name**,
2. **normalised name** — case-folded, runs of whitespace collapsed (`"Bass  Out"`
   = `"bass out"`, but `"Bass_Out"` ≠ `"Bass Out"`),
3. **interleave index**.

There is no fuzzy matching. The result carries a mapping report:

```json
{ "matched": [ { "name": "Bass_Out", "index": 1,  "by": "name",  "template_name": "Bass_Out" },
               { "name": "Kanal 12", "index": 12, "by": "index", "template_name": "Kanal 12 alt" } ],
  "unknown": ["Neue Spur"],
  "missing": ["Keys_O S L"] }
```

- `matched` — tracks of the recording that got template parameters, in
  recording order; `by` is `name`, `normalized` or `index`, `template_name`
  is what the template called it.
- `unknown` — tracks of the recording the template had no entry for. The
  caller chooses what happens to them: `exclude` (out of the mix — the
  default for templates, because a new channel is as likely a monitor feed as
  a source) or `include` (unity, centred, or hard L/R for a trailing ` L`/` R`).
- `missing` — template tracks the recording does not have.

A partial mapping is not an error. A mapping where `matched` is empty means
the template belongs to a different setup; the CLI refuses to render that
(exit code 4).
