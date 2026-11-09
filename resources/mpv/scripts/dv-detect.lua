-- Windows presentation-profile selection.
--
-- mpv on Windows can present video two ways, and the correct one is a
-- property of the CONTENT, not of the transition between files:
--   * "default" — the normal DXGI flip-model swapchain (d3d11-flip=yes).
--     DWM handles HDR/SDR passthrough and tone-mapping automatically.
--   * "dolby_vision" — DWM-composited, non-flip presentation
--     (d3d11-flip=no). Required for Dolby Vision: DWM's own automatic DV
--     reprocessing is what actually signals real DV over HDMI, and that
--     reprocessing only engages for DWM-composited content. Using it for
--     anything else has DWM upconvert HDR10/SDR into DV too — tried and
--     rejected during development (see repo/PR history).
--
-- Both mpv options this drives (`target-colorspace-hint`, `d3d11-flip`) are
-- set as plain globals, and this script is their sole writer — nothing else
-- in the app touches them (an equivalent Rust-side path existed early in
-- development and was removed for the same reason). Profiles are asserted
-- unconditionally relative to whichever profile is currently applied — never left to mpv's
-- own state to carry forward or revert. `file-local-options/` was tried
-- first; its auto-revert-on-file-end races the VO-recreate/rebind it
-- triggers, and losing that race left flip=no applied to non-DV content
-- after a DV file ended (fullscreen getting stuck presenting DV instead of
-- falling back to HDR — timing-sensitive enough to differ between TVs with
-- different HDMI mode-switch latency). It was also unsafe together with the
-- do-nothing-on-repeat guard below: two consecutive DV files would have the
-- first file's override auto-revert before the second file's own
-- on_preloaded ran, and the second file — same profile as the first — would
-- skip reasserting it, silently losing DV presentation. Plain globals, owned
-- entirely by `apply_profile` below, have no implicit revert to race or to
-- be defeated by that guard.
--
-- Changing d3d11-flip on a live VO forces mpv to tear down and recreate its
-- own native render window — the Rust side observes mpv's window-id
-- property and rebinds the compositor, input child window, WndProc hook,
-- and SMTC to the new window when that happens
-- (src/windows/src/platform.rs, win_on_window_handle_changed). Do not set
-- d3d11-flip outside `apply_profile`'s do-nothing-on-repeat guard without
-- that recovery plumbing in place: a file that loops by reloading itself
-- (e.g. a details-page theme/backdrop video) re-fires on_preloaded every
-- loop even when the profile hasn't changed, and re-writing d3d11-flip to
-- the value it already holds still forces a VO tear-down/recreate — every
-- loop iteration was triggering a full window-recreate/rebind cycle,
-- leaking resources far faster than an actual profile transition would.
-- The guard is keyed on (profile, path) as of 2026-08-04, not profile
-- alone — see `apply_profile`'s own comment for why a name-only guard
-- silently broke DV->non-DV recovery on G5.
--
-- mpv's own scripts-directory auto-load is left at its default (on) so
-- users' own scripts and mpv.conf settings keep working normally — see
-- mpv/src/boot.rs. That means a stray manually-placed copy of this exact
-- file sitting in a user's mpv config scripts/ directory (e.g. left over
-- from earlier manual testing) could genuinely auto-load a second time
-- alongside the app's bundled copy, so this script claims a marker in
-- mpv's `user-data/` property tree — shared across every script instance
-- in the same core — and any later instance no-ops immediately instead of
-- registering its own on_preloaded hook. That's a real scenario this has
-- hit before: two instances double-writing target-colorspace-hint around
-- FILE_LOADED raced the same way the old Rust+Lua dual mechanism did on
-- 2026-07-10 and corrupted fragile dual-layer DV/HEVC decode ("PPS changed
-- between slices").
--
-- Known limitation: classification takes "any video track has a Dolby
-- Vision profile" as its signal. For the overwhelmingly common case of a
-- single video track this is exact. A file with multiple video tracks
-- where the selected one differs in DV-ness from an unselected alternate
-- (e.g. a multi-angle disc rip) could misclassify — deliberately accepted
-- since mpv's track-list reports every track as unselected at the point
-- this runs (see classify()), so there is no reliable "selected" signal
-- available yet to narrow this further.
--
-- A DV->non-DV transition in fullscreen can leave the display stuck
-- showing DV even though the properties above are correctly reasserted to
-- non-DV (reproduced every time on an LG G5 — see
-- kick_fullscreen_to_clear_stuck_dv below). That's believed to be DWM not
-- re-evaluating its automatic DV engagement just because mpv's swapchain
-- changed, not anything wrong in this script's own logic.

if mp.get_property("platform") ~= "windows" then
    return
end

-- Claim this instance's slot in mpv's shared user-data/ tree. If another
-- instance of this same script already claimed it in this mpv core (see
-- header comment), this instance stops here — no hook registered, no
-- options ever touched by it.
if mp.get_property_native("user-data/dv-detect/claimed", false) then
    mp.msg.warn(
        "[dv-detect] another instance of this script already loaded in this " ..
        "mpv core — skipping duplicate load")
    return
end
mp.set_property_native("user-data/dv-detect/claimed", true)

-- Presentation profile as data: extending this to a third profile (should
-- one ever turn out to need distinct handling) is a new table entry plus a
-- new classify() branch — apply_profile()'s correctness guarantees don't
-- change.
local PROFILES = {
    dolby_vision = { hint = "no", flip = "no" },
    default = { hint = "yes", flip = "yes" },
}

-- mpv reports every track as unselected at the point on_preloaded runs
-- (before track selection happens), so "selected" can't be used to narrow
-- this — any video track carrying Dolby Vision metadata is enough to
-- classify the whole file as Dolby Vision.
local function classify(tracks)
    for _, track in ipairs(tracks) do
        if track.type == "video" and track["dolby-vision-profile"] then
            return "dolby_vision"
        end
    end
    return "default"
end

local applied_profile = nil
local applied_path = nil

-- The only place that writes target-colorspace-hint/d3d11-flip. No-ops only
-- when BOTH `name` and `path` already match what's applied — i.e. the exact
-- same file re-triggering on_preloaded (a looping theme/backdrop video
-- reloading itself), which is what this guard exists to protect against
-- (see header comment). Deliberately NOT keyed on `name` alone: two
-- genuinely different files that happen to classify to the same profile
-- back-to-back must each still get a fresh, unconditional assertion.
--
-- Found 2026-08-04 (G5, fullscreen): The Martian (HDR, "default") -> a
-- Dolby Vision film ("dolby_vision") -> a third, different file that also
-- classified "default" -> The Martian again ("default"). The old
-- name-only guard saw "default" already applied (from the third file) and
-- silently skipped reasserting it for The Martian's reload — with no way
-- to know whether that third file's own "default" assertion had actually
-- taken effect on the display (G5's HDMI/driver stack doesn't reliably
-- honor a single DV->non-DV re-assertion). The Martian was then stuck showing DV with
-- nothing left to ever retry the assertion, since nothing else was going
-- to write target-colorspace-hint/d3d11-flip again until the classified
-- profile changed. Keying on path too means a genuinely different piece
-- of content is never silently trusted to already match just because some
-- other file recently requested the same profile.
-- 2026-08-04/05 finding, confirmed deterministic on G5: a DV file followed
-- by a non-DV file in fullscreen leaves the TV stuck showing DV 100% of
-- the time, even though target-colorspace-hint/d3d11-flip are correctly
-- reasserted to "default" (confirmed via `colorspace[...]` diagnostic
-- logging — mpv's own recorded state matches what was
-- requested; the mismatch is downstream, presumably DWM not re-evaluating
-- its automatic DV engagement for the output just because mpv's own
-- swapchain changed). User independently confirmed, before this fix
-- existed, that manually toggling out of fullscreen and back always
-- clears it — this kicks that same real toggle automatically so nobody
-- has to do it by hand. Deferred a beat after the profile write so it
-- doesn't race the VO tear-down/recreate that d3d11-flip's own change
-- already triggers (see module header comment + platform.rs
-- win_on_window_handle_changed) — that recreate needs to settle and get
-- rebound on the Rust side first.
local FULLSCREEN_KICK_DELAY_SECONDS = 1.0
local FULLSCREEN_KICK_REENTER_DELAY_SECONDS = 0.35

local function kick_fullscreen_to_clear_stuck_dv()
    if not mp.get_property_native("fullscreen", false) then
        return
    end
    mp.msg.info("[dv-detect] DV->non-DV transition while fullscreen: cycling fullscreen to clear a possibly-stuck DV output state")
    mp.set_property_native("fullscreen", false)
    mp.add_timeout(FULLSCREEN_KICK_REENTER_DELAY_SECONDS, function()
        mp.set_property_native("fullscreen", true)
    end)
end

local function apply_profile(name, path)
    if name == applied_profile and path == applied_path then
        return
    end
    local previous_profile = applied_profile
    applied_profile = name
    applied_path = path

    local profile = PROFILES[name]
    mp.set_property("target-colorspace-hint", profile.hint)
    mp.set_property("d3d11-flip", profile.flip)
    mp.msg.info(string.format(
        "[dv-detect] presentation profile -> %s (target-colorspace-hint=%s d3d11-flip=%s)",
        name, profile.hint, profile.flip))

    if previous_profile == "dolby_vision" and name == "default" then
        mp.add_timeout(FULLSCREEN_KICK_DELAY_SECONDS, kick_fullscreen_to_clear_stuck_dv)
    end
end

mp.add_hook("on_preloaded", 50, function()
    local tracks = mp.get_property_native("track-list", {})
    apply_profile(classify(tracks), mp.get_property("path"))
end)
