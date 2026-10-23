#!/usr/bin/env python3
# -*- coding: utf-8 -*-
r"""
register_accc_pim.py  --  record the extracted ACCCx packages the way Adobe's installer does
============================================================================================

WHY
---
extract_accc_runtime.py lays the Creative Cloud Desktop runtime's `.pima` packages onto disk. Adobe's own
installer does one more thing per package: its PIM engine (Adobe Desktop Common\Core\AdobePIM.dll) records the
package in a `pim.db` (SQLite) at the package set's install path, with the applets the package's `.pimx`
declares, and keeps the `.pimx` next to the files. Adobe Desktop Service (ADS) reads those records at start
(AdobePIM `pim_getAppletAndPackageInfo`); with no `pim.db` it logs "Unable to read CC Version from pim
database ... Failed to get applet [Applet Id - NHEX_UIX]. Showing corruption dialog..." and shows
"Adobe Creative Cloud, needed to resolve the problem, is missing or damaged" [Quit] [Repair] every time
Photoshop starts (his words, 2026-10-23: "It appears when launching Photoshop even when signed in").

WHAT
----
For every package of every packageSet in ApplicationInfo.xml whose files are on disk
(<resolved installPath><target_location>), from its `.pimx`:
    package_installation_info  (packageid, version, name, target_location)
    package_upgrade_info       (packageid, old_packageid)            -- <package_upgrade_info>
    applet_registration_info   (name, type, version, display_name, module_path, packageid)
    applet_relationship_info   (name, type, version, relationship, relative_*, packageid)
    journal                    one NewInstall/success row per package
    pim_meta                   schema_version 4, schema_compatibility_version 1, application_version
into <resolved installPath>\pim.db, with the table layout of a pim.db Adobe's installer wrote (schema 4), and
copies the `.pimx` to <install dir>\<name>.pimx. Install paths come from extract_accc_runtime.py's TOKEN_MAP,
so the records describe the files where they actually are (a 64-bit set's in the 64-bit root, as
on Windows: a reference install keeps CEF64/NGL in C:\Program Files\Common Files\...\pim.db). Idempotent: re-running replaces rows with the same
values and adds no second journal row.

Orchestration only, like the extractor: it reads Adobe's manifests and writes the records Adobe's installer
writes; no Adobe binary is changed. Writes stay inside <prefix>/drive_c.

USAGE
    register_accc_pim.py --packages <ACCCx packages dir> --prefix <WINEPREFIX or drive_c> [--sets AAM,ADC,ADC64] [--dry-run]
Stdlib only.
"""

import argparse
import datetime
import os
import re
import shutil
import sqlite3
import sys
import uuid
import xml.etree.ElementTree as ET

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from extract_accc_runtime import TOKEN_MAP, TOKEN_64  # noqa: E402  (the same install paths the files went to)

SCHEMA = [
    "CREATE TABLE IF NOT EXISTS package_installation_info ( packageid char(36), version char(16), name  varchar(128), "
    "target_location varchar(512), constraint pk PRIMARY KEY (packageid) )",
    "CREATE TABLE IF NOT EXISTS package_upgrade_info ( packageid char(36), old_packageid char(36), constraint pk "
    "PRIMARY KEY (packageid, old_packageid) )",
    "CREATE TABLE IF NOT EXISTS applet_registration_info ( name char(16), type char(8), version char(16), display_name "
    "varchar(128), module_path varchar(512), packageid char(36) references package_installation_info ( packageid ), "
    "constraint pk PRIMARY KEY (name, type, version) )",
    "CREATE TABLE IF NOT EXISTS journal ( pkg_name text, pkg_id char(36), time_action char(32), action char(16), pkg_type "
    "char(16), current_status char(16), info text, uid char(36), constraint pk PRIMARY KEY (uid) )",
    "CREATE TABLE IF NOT EXISTS pim_meta ( key text, value text, constraint pk PRIMARY KEY (key) )",
    "CREATE TABLE IF NOT EXISTS applet_relationship_info ( name char(16), type char(8), version char(16), relationship "
    "varchar(32), relative_name char(16), relative_type char(8) ,relative_version char(16),relative_order char(8),"
    "packageid char(36) references package_installation_info ( packageid ), constraint pk PRIMARY KEY (name, type, "
    "version,relationship,relative_name,relative_type,relative_version) )",
    "CREATE TABLE IF NOT EXISTS package_uninstallation_info ( name  varchar(128) )",
    "CREATE INDEX IF NOT EXISTS AppletRegistrationInfo_appletName_index ON applet_registration_info ( name )",
    "CREATE INDEX IF NOT EXISTS PackageUpgradeInfo_oldPackageid_index ON package_upgrade_info ( old_packageid )",
    "CREATE INDEX IF NOT EXISTS PackageUpgradeInfo_packageid_index ON package_upgrade_info ( packageid )",
    "CREATE INDEX IF NOT EXISTS AppletRegistrationInfo_packageid_index ON applet_registration_info ( packageid )",
    "CREATE INDEX IF NOT EXISTS Jouranal_packageid_index ON journal ( pkg_id )",
    "CREATE INDEX IF NOT EXISTS Jouranal_packageName_index ON journal ( pkg_name )",
    "CREATE INDEX IF NOT EXISTS PackageInstallationInfo_packageName_index ON package_installation_info ( name )",
]


def win_to_host(drive_c, win_path):
    """C:\\a\\b -> <drive_c>/a/b"""
    rest = re.sub(r"^[A-Za-z]:\\", "", win_path).replace("\\", "/")
    return os.path.join(drive_c, rest)


def text(el, tag):
    t = el.find(tag)
    return (t.text or "").strip() if t is not None and t.text else ""


def package_sets(app_info):
    root = ET.parse(app_info).getroot()
    for ps in root.iter("packageSet"):
        name, token = text(ps, "name"), text(ps, "installPath")
        pkgs = [text(p, "name") for p in ps.iter("package") if text(p, "name")]
        yield name, token, pkgs


def read_pimx(path):
    root = ET.parse(path).getroot()
    info = {k: text(root, k) for k in ("packageid", "name", "version", "target_location")}
    info["upgrades"] = [o.text.strip() for o in root.iter("old_packageid") if o.text]
    info["applets"] = [{k: text(a, k) for k in ("name", "type", "version", "display_name", "module_path")}
                       for a in root.iter("applet_registration_element")]
    info["relations"] = [{k: text(r, k) for k in ("name", "type", "version", "relationship", "relative_name",
                                                  "relative_type", "relative_version", "relative_order")}
                         for r in root.iter("applet_relationship_element")]
    return info


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[1])
    ap.add_argument("--packages", required=True)
    ap.add_argument("--prefix", required=True)
    ap.add_argument("--sets", help="comma list of packageSets to record (default: all on disk)")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    drive_c = a.prefix if os.path.basename(os.path.normpath(a.prefix)) == "drive_c" else os.path.join(a.prefix, "drive_c")
    app_info = os.path.join(a.packages, "ApplicationInfo.xml")
    version = text(ET.parse(app_info).getroot(), "version") or ""
    if not version:
        m = re.search(r"/(\d+\.\d+\.\d+\.\d+)/", os.path.abspath(a.packages) + "/")
        version = m.group(1) if m else ""
    dbs, total, skipped = {}, 0, 0
    now = datetime.datetime.now()
    stamp = f"{now.month}/{now.day}/{now.year} {now:%H:%M:%S}"
    want = {x.strip().upper() for x in a.sets.split(",")} if a.sets else None
    for set_name, token, pkgs in package_sets(app_info):
        if want is not None and set_name.upper() not in want:
            continue
        # as extract_accc_runtime.py resolves it: a 64-bit set goes to the 64-bit root
        token = TOKEN_64.get(token, token) if set_name.endswith("64") else token
        if token not in TOKEN_MAP:
            print(f"packageSet {set_name}: unknown install path {token}, skipped", file=sys.stderr)
            continue
        base = win_to_host(drive_c, TOKEN_MAP[token])
        for pkg in pkgs:
            pimx = os.path.join(a.packages, set_name, pkg, pkg + ".pimx")
            if not os.path.isfile(pimx):
                continue
            info = read_pimx(pimx)
            target = os.path.join(base, info["target_location"].strip("/"))
            if not os.path.isdir(target):
                skipped += 1
                continue
            total += 1
            print(f"{set_name:6} {info['name']:18} {info['version']:12} -> {TOKEN_MAP[token]}\\pim.db "
                  f"({len(info['applets'])} applet(s){', ' + ', '.join(x['name'] + '_' + x['type'] for x in info['applets']) if info['applets'] else ''})")
            if a.dry_run:
                continue
            if base not in dbs:
                db = sqlite3.connect(os.path.join(base, "pim.db"))
                for s in SCHEMA:
                    db.execute(s)
                for k, v in (("schema_version", "4"), ("schema_compatibility_version", "1"), ("application_version", version)):
                    db.execute("INSERT OR REPLACE INTO pim_meta VALUES (?, ?)", (k, v))
                dbs[base] = db
            db = dbs[base]
            pid = info["packageid"]
            db.execute("INSERT OR REPLACE INTO package_installation_info VALUES (?, ?, ?, ?)",
                       (pid, info["version"], info["name"], info["target_location"]))
            for old in info["upgrades"]:
                db.execute("INSERT OR REPLACE INTO package_upgrade_info VALUES (?, ?)", (pid, old))
            for x in info["applets"]:
                db.execute("INSERT OR REPLACE INTO applet_registration_info VALUES (?, ?, ?, ?, ?, ?)",
                           (x["name"], x["type"], x["version"], x["display_name"], x["module_path"], pid))
            for r in info["relations"]:
                db.execute("INSERT OR REPLACE INTO applet_relationship_info VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                           (r["name"], r["type"], r["version"], r["relationship"], r["relative_name"], r["relative_type"],
                            r["relative_version"], r["relative_order"], pid))
            if not db.execute("SELECT 1 FROM journal WHERE pkg_id = ? AND action = 'NewInstall'", (pid,)).fetchone():
                db.execute("INSERT INTO journal VALUES (?, ?, ?, 'NewInstall', '', 'success', ?, ?)",
                           (info["name"], pid, stamp, info["version"], "{" + str(uuid.uuid4()).upper() + "}"))
            dest = os.path.join(target, info["name"] + ".pimx")
            if not os.path.exists(dest):
                shutil.copyfile(pimx, dest)
    for db in dbs.values():
        db.commit()
        db.close()
    print(f"{total} package(s) {'would be ' if a.dry_run else ''}recorded in {len(dbs) or '(dry run)'} pim.db; "
          f"{skipped} not on disk, skipped")


if __name__ == "__main__":
    main()
