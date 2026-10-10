#!/usr/bin/env python3
# -*- coding: utf-8 -*-
r"""
extract_accc_runtime.py  --  Mud Hut ACCCx runtime extractor
=============================================================

WHAT THIS DOES
--------------
Adobe ships the Creative Cloud Desktop ("ACCCx") runtime as a set of
`.pima` packages -- each `.pima` is a plain ZIP archive whose entries are
laid out relative to a per-component install directory.  Next to every
`.pima` sits a `.pimx` (an XML manifest) that declares, among other things,
the authoritative `<target_location>` sub-path the archive extracts into.
A master manifest, `ApplicationInfo.xml`, groups the packages into
`packageSet`s and gives each set an `<installPath>` written as an
unexpanded token: `[AAM_PATH]`, `[ADC_PATH]`, or `[ACC_PATH]`.

This tool reproduces -- purely as file-copy orchestration -- what Adobe's
own HyperDrive/ACCC installer does when it lays the runtime down onto disk:
it resolves the tokens to the real Windows paths inside a Wine prefix and
unzips each selected `.pima` into
        <resolved installPath>\<pimx target_location>\

TOKEN MAP  (resolved, then confirmed against a known-good reference prefix)
--------------------------------------------------------------------------
    [ADC_PATH] = C:\Program Files (x86)\Common Files\Adobe\Adobe Desktop Common
                 (ADC + ADC64 packageSets -- the shared "Desktop Common" tree;
                  e.g. HDBox\HDPIM.dll, ADS\CRClient.dll, ElevationManager\...)
    [ACC_PATH] = C:\Program Files (x86)\Adobe\Adobe Creative Cloud
                 (ACC + ACC64 packageSets -- the CC-desktop app itself;
                  e.g. ACC\Creative Cloud.exe)
    [AAM_PATH] = C:\Program Files (x86)\Common Files\Adobe
                 (AAM packageSet; IPC\AdobeIPCBroker.exe).  ADC_PATH sits
                 directly beneath this dir, matching Adobe's convention of
                 %CommonProgramFiles(x86)%\Adobe for the AAM token.

Note that both 32- and 64-bit packageSets share a token: ADC64.installPath and
ADC.installPath are BOTH [ADC_PATH], and ACC64/ACC are BOTH [ACC_PATH].  The
64-bit components therefore do NOT get their own "*64" directory -- the `.pimx`
`<target_location>` folds them into the 32-bit component's dir (Core64 -> /Core/,
CEF64 -> /CEF/, HEX64 -> /HEX/, CoreExt64 -> /CoreExt/, TCC64 -> /TCC/,
UPI64 -> /RemoteComponents/UPI/).  This is exactly why we trust the per-package
`<target_location>` from the `.pimx` rather than the package name.

LEGAL / SCOPE NOTE
------------------
This is orchestration only.  It extracts *pristine, unmodified* Adobe `.pima`
archives (plain ZIPs) into the standard Windows directory layout.  It does NOT
modify, patch, decrypt, reimplement, or repackage any Adobe binary; it does not
bundle Adobe content; it reads packages the operator already staged.  It only
copies bytes out of a ZIP into the folders Adobe's own installer would use.
It never writes outside <prefix>/drive_c.

USAGE
-----
    extract_accc_runtime.py --packages <dir> --prefix <WINEPREFIX>
        [--sets ADC,AAM,ACC,ACC64,ADC64]
        [--only HDBox,ADS,Core,...]
        [--dry-run] [--verbose]

    --packages   Directory holding the staged packageSets (AAM/ ADC/ ...) and
                 ApplicationInfo.xml.
    --prefix     WINEPREFIX (a dir containing drive_c/), or a drive_c dir
                 itself.  All writes are confined to <prefix>/drive_c.
    --sets       Comma list of packageSets to process (default: all present).
    --only       Comma list of component/package names to restrict to
                 (e.g. the INSTALL leg needs only HDBox).  Case-insensitive.
    --dry-run    Resolve + report what WOULD be extracted, write nothing.
    --verbose    Per-file logging.

Exit status is non-zero on any unresolved token, missing `.pima`, bad ZIP, or
attempted path escape.

Stdlib only.
"""

import argparse
import os
import sys
import zipfile
import xml.etree.ElementTree as ET
from pathlib import PurePosixPath, PureWindowsPath


# ---------------------------------------------------------------------------
# Token -> real Windows path.  Values are Windows-style absolute paths; they
# get mapped onto <drive_c> at resolve time.  Keys are the *exact* tokens that
# appear inside <installPath> in ApplicationInfo.xml.
# ---------------------------------------------------------------------------
TOKEN_MAP = {
    "[AAM_PATH]": r"C:\Program Files (x86)\Common Files\Adobe",
    "[ADC_PATH]": r"C:\Program Files (x86)\Common Files\Adobe\Adobe Desktop Common",
    "[ACC_PATH]": r"C:\Program Files (x86)\Adobe\Adobe Creative Cloud",
    # 64-bit packageSets (ADC64/ACC64) install to the *64-bit* roots, NOT (x86).
    # Verified against a known-good prefix: 64-bit Common Files holds the ADC64
    # components (Core/HEX/CEF/NGL 64-bit) that the 64-bit Creative Cloud.exe loads;
    # (x86) holds the 32-bit ADC set. Both must be laid down.
    "[ADC_PATH_64]": r"C:\Program Files\Common Files\Adobe\Adobe Desktop Common",
    "[ACC_PATH_64]": r"C:\Program Files\Adobe\Adobe Creative Cloud",
}

# 64-bit packageSets remap their (shared) token to the 64-bit-root variant.
TOKEN_64 = {"[ADC_PATH]": "[ADC_PATH_64]", "[ACC_PATH]": "[ACC_PATH_64]"}

# Fallback token per packageSet name, used only if ApplicationInfo.xml is
# absent and we have to walk the tree.  Mirrors the manifest's installPath.
SET_TOKEN_FALLBACK = {
    "AAM": "[AAM_PATH]",
    "ADC": "[ADC_PATH]",
    "ADC64": "[ADC_PATH_64]",
    "ACC": "[ACC_PATH]",
    "ACC64": "[ACC_PATH_64]",
}


def log(msg):
    sys.stderr.write(msg + "\n")


def vlog(enabled, msg):
    if enabled:
        sys.stderr.write(msg + "\n")


class ExtractError(Exception):
    pass


# ---------------------------------------------------------------------------
# Prefix / path resolution
# ---------------------------------------------------------------------------
def resolve_drive_c(prefix):
    """Return the (possibly non-existent) drive_c path for a prefix arg."""
    prefix = os.path.abspath(prefix)
    cand = os.path.join(prefix, "drive_c")
    if os.path.isdir(cand):
        return cand
    if os.path.basename(prefix.rstrip("/")) == "drive_c":
        return prefix
    # default: assume standard layout even if it doesn't exist yet (dry-run)
    return cand


def win_to_fs(drive_c, win_path):
    """Map a 'C:\\Program Files (x86)\\...' path onto <drive_c>/..."""
    p = PureWindowsPath(win_path)
    # Drop the drive anchor (e.g. 'C:\\'); join the rest under drive_c.
    parts = p.parts
    if parts and (len(parts[0]) == 2 and parts[0].endswith(":") or parts[0].endswith(":\\")):
        parts = parts[1:]
    return os.path.join(drive_c, *parts)


def target_subdir_from_pimx(pimx_path):
    """Read <target_location> from a .pimx; return a clean relative sub-path."""
    try:
        tree = ET.parse(pimx_path)
    except ET.ParseError as e:
        raise ExtractError("bad .pimx XML %s: %s" % (pimx_path, e))
    el = tree.getroot().find("target_location")
    if el is None or not (el.text or "").strip():
        return None
    # e.g. "/Components/3DI/" -> "Components/3DI"
    return PurePosixPath(el.text.strip().replace("\\", "/")).relative_to("/") \
        if el.text.strip().startswith("/") \
        else PurePosixPath(el.text.strip().replace("\\", "/"))


# ---------------------------------------------------------------------------
# Manifest parsing
# ---------------------------------------------------------------------------
def parse_application_info(app_info_path):
    """
    Parse ApplicationInfo.xml.
    Return Ordered-ish list of (set_name, token, [(pkg_name, pimx_rel), ...]).
    pimx_rel is the path as written in <pimxPath> (leading-slash, POSIX).
    """
    tree = ET.parse(app_info_path)
    root = tree.getroot()
    result = []
    for ps in root.iter("packageSet"):
        name_el = ps.find("name")
        ipath_el = ps.find("installPath")
        if name_el is None or ipath_el is None:
            continue
        set_name = (name_el.text or "").strip()
        token = (ipath_el.text or "").strip()
        pkgs = []
        pkgs_parent = ps.find("packages")
        if pkgs_parent is not None:
            for pkg in pkgs_parent.findall("package"):
                pn = pkg.find("name")
                px = pkg.find("pimxPath")
                if pn is None or px is None:
                    continue
                pkgs.append(((pn.text or "").strip(), (px.text or "").strip()))
        result.append((set_name, token, pkgs))
    return result


def discover_by_walk(packages_dir):
    """Fallback: walk packages/<SET>/<COMP>/<COMP>.pimx when no manifest."""
    result = []
    for set_name in sorted(os.listdir(packages_dir)):
        set_dir = os.path.join(packages_dir, set_name)
        if not os.path.isdir(set_dir):
            continue
        token = SET_TOKEN_FALLBACK.get(set_name)
        if not token:
            continue
        pkgs = []
        for comp in sorted(os.listdir(set_dir)):
            comp_dir = os.path.join(set_dir, comp)
            if not os.path.isdir(comp_dir):
                continue
            pimx = os.path.join(comp_dir, comp + ".pimx")
            if os.path.isfile(pimx):
                pkgs.append((comp, "/%s/%s/%s.pimx" % (set_name, comp, comp)))
        if pkgs:
            result.append((set_name, token, pkgs))
    return result


# ---------------------------------------------------------------------------
# Extraction
# ---------------------------------------------------------------------------
def safe_join(root_real, rel_parts):
    """Join and guarantee the result stays under root_real (no .. / abs escape)."""
    dest = os.path.normpath(os.path.join(root_real, *rel_parts))
    # normpath collapses '..'; verify containment lexically (works for
    # non-existent paths during dry-run too).
    root_norm = os.path.normpath(root_real)
    if dest != root_norm and not dest.startswith(root_norm + os.sep):
        raise ExtractError("path escape blocked: %s" % dest)
    return dest


def plan_component(packages_dir, drive_c, set_name, token, pkg_name, pimx_rel):
    """
    Resolve everything for one component. Return a dict describing the plan,
    or raise ExtractError.
    """
    # 64-bit packageSets share the 32-bit token in ApplicationInfo.xml but must
    # install to the 64-bit roots (the 64-bit Creative Cloud.exe loads its Core/
    # HEX/CEF/NGL from C:\Program Files\Common Files\..., not (x86)).
    if set_name.endswith("64"):
        token = TOKEN_64.get(token, token)
    if token not in TOKEN_MAP:
        raise ExtractError("unresolved installPath token %r (set %s)" % (token, set_name))

    pimx_path = os.path.join(packages_dir, pimx_rel.lstrip("/"))
    if not os.path.isfile(pimx_path):
        raise ExtractError("missing .pimx: %s" % pimx_path)
    pima_path = pimx_path[:-5] + ".pima" if pimx_path.endswith(".pimx") \
        else os.path.join(os.path.dirname(pimx_path), pkg_name + ".pima")
    if not os.path.isfile(pima_path):
        raise ExtractError("missing .pima for package %s: %s" % (pkg_name, pima_path))

    subdir = target_subdir_from_pimx(pimx_path)
    if subdir is None:
        subdir = PurePosixPath(pkg_name)  # fallback to package name
    win_install = TOKEN_MAP[token]
    install_fs = win_to_fs(drive_c, win_install)
    target_fs = os.path.join(install_fs, *subdir.parts)
    win_target = str(PureWindowsPath(win_install) / PureWindowsPath(*subdir.parts))
    return {
        "set": set_name,
        "package": pkg_name,
        "pima": pima_path,
        "target_subdir": str(subdir),
        "win_target": win_target,
        "target_fs": target_fs,
    }


def extract_pima(plan, drive_c_real, dry_run, verbose):
    """
    Extract one planned component. Return (n_files, total_bytes, n_written).
    """
    n_files = 0
    total_bytes = 0
    n_written = 0
    try:
        zf = zipfile.ZipFile(plan["pima"])
    except zipfile.BadZipFile as e:
        raise ExtractError("bad ZIP %s: %s" % (plan["pima"], e))
    with zf:
        bad = zf.testzip() if verbose else None
        if bad:
            raise ExtractError("corrupt entry %s in %s" % (bad, plan["pima"]))
        for info in zf.infolist():
            name = info.filename
            # normalize, reject absolute / traversal
            rel = name.replace("\\", "/")
            if rel.startswith("/") or ".." in PurePosixPath(rel).parts:
                raise ExtractError("unsafe zip entry %r in %s" % (name, plan["pima"]))
            is_dir = rel.endswith("/")
            dest = safe_join(drive_c_real, [os.path.relpath(plan["target_fs"], drive_c_real)]
                             + list(PurePosixPath(rel).parts))
            if is_dir:
                if not dry_run:
                    os.makedirs(dest, exist_ok=True)
                continue
            n_files += 1
            total_bytes += info.file_size
            if dry_run:
                continue
            # idempotent skip: same size already present
            if os.path.isfile(dest) and os.path.getsize(dest) == info.file_size:
                vlog(verbose, "    skip (present) %s" % rel)
                continue
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with zf.open(info) as src, open(dest, "wb") as out:
                while True:
                    chunk = src.read(1 << 20)
                    if not chunk:
                        break
                    out.write(chunk)
            n_written += 1
            vlog(verbose, "    write %s" % rel)
    return n_files, total_bytes, n_written


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Extract Adobe ACCCx .pima runtime packages into a Wine prefix.")
    ap.add_argument("--packages", required=True,
                    help="Dir with staged packageSets + ApplicationInfo.xml")
    ap.add_argument("--prefix", required=True,
                    help="WINEPREFIX (containing drive_c) or a drive_c dir")
    ap.add_argument("--sets", default=None,
                    help="Comma list of packageSets (default: all present)")
    ap.add_argument("--only", default=None,
                    help="Comma list of component names to restrict to")
    ap.add_argument("--dry-run", action="store_true",
                    help="Resolve + report only; write nothing")
    ap.add_argument("--verbose", action="store_true", help="Per-file logging")
    args = ap.parse_args(argv)

    packages_dir = os.path.abspath(args.packages)
    if not os.path.isdir(packages_dir):
        log("ERROR: --packages not a directory: %s" % packages_dir)
        return 2

    drive_c = resolve_drive_c(args.prefix)
    # For containment checks we need a real anchor; if drive_c exists use its
    # realpath, else use the normalized (non-existent) path -- writes won't
    # happen in that case anyway (dry-run against a fake prefix).
    drive_c_real = os.path.realpath(drive_c) if os.path.exists(drive_c) \
        else os.path.normpath(drive_c)

    if not args.dry_run and not os.path.isdir(drive_c):
        log("ERROR: prefix drive_c does not exist (use --dry-run to preview): %s"
            % drive_c)
        return 2

    sets_filter = None
    if args.sets:
        sets_filter = set(s.strip() for s in args.sets.split(",") if s.strip())
    only_filter = None
    if args.only:
        only_filter = set(s.strip().lower() for s in args.only.split(",") if s.strip())

    # Build the package list.
    app_info = os.path.join(packages_dir, "ApplicationInfo.xml")
    try:
        if os.path.isfile(app_info):
            sets = parse_application_info(app_info)
            vlog(args.verbose, "Parsed manifest: %s" % app_info)
        else:
            log("WARNING: ApplicationInfo.xml not found; walking package tree.")
            sets = discover_by_walk(packages_dir)
    except ET.ParseError as e:
        log("ERROR: cannot parse ApplicationInfo.xml: %s" % e)
        return 2

    if not sets:
        log("ERROR: no packageSets discovered under %s" % packages_dir)
        return 2

    log("ACCCx runtime extractor")
    log("  packages : %s" % packages_dir)
    log("  drive_c  : %s%s" % (drive_c, "  (DRY-RUN)" if args.dry_run else ""))

    grand_files = grand_bytes = grand_written = grand_comps = 0
    errors = []

    for set_name, token, pkgs in sets:
        if sets_filter is not None and set_name not in sets_filter:
            continue
        selected = [(pn, px) for (pn, px) in pkgs
                    if only_filter is None or pn.lower() in only_filter]
        if not selected:
            continue
        eff_token = TOKEN_64.get(token, token) if set_name.endswith("64") else token
        log("== packageSet %s  ->  %s (%s)" % (set_name, TOKEN_MAP.get(eff_token, "??"), eff_token))
        for pkg_name, pimx_rel in selected:
            try:
                # Plan against drive_c_real, the same anchor the containment check uses.
                # With the unresolved path, a home behind a symlink (Bazzite, Silverblue and
                # other Fedora Atomic desktops: /home -> /var/home) made every target look like
                # an escape: "path escape blocked" for all 60 components, nothing extracted.
                plan = plan_component(packages_dir, drive_c_real, set_name, token,
                                      pkg_name, pimx_rel)
                nf, nb, nw = extract_pima(plan, drive_c_real, args.dry_run, args.verbose)
            except ExtractError as e:
                log("  ERROR [%s/%s]: %s" % (set_name, pkg_name, e))
                errors.append((set_name, pkg_name, str(e)))
                continue
            grand_comps += 1
            grand_files += nf
            grand_bytes += nb
            grand_written += nw
            verb = "would extract" if args.dry_run else ("wrote %d, %d already present"
                                                          % (nw, nf - nw))
            log("  %-18s -> %-40s  %5d files  %12d bytes  [%s]"
                % (pkg_name, plan["target_subdir"], nf, nb, verb))

    log("-" * 78)
    log("TOTAL: %d components, %d files, %d bytes%s"
        % (grand_comps, grand_files, grand_bytes,
           "" if args.dry_run else (", %d newly written" % grand_written)))

    if only_filter is not None:
        matched = set()
        for _, _, pkgs in sets:
            for pn, _ in pkgs:
                if pn.lower() in only_filter:
                    matched.add(pn.lower())
        missing = only_filter - matched
        if missing:
            log("ERROR: --only names not found in manifest: %s" % ", ".join(sorted(missing)))
            errors.append(("--only", ",".join(sorted(missing)), "not found"))

    if errors:
        log("FAILED with %d error(s)." % len(errors))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
