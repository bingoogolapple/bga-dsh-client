"""Verify the real Linux client before and after a toolbar refresh.

Runs inside the smoke test's isolated Xvfb/D-Bus session. No browser cookies,
launch tokens, or OCR text are written to the log.
"""

import argparse
from collections import deque
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

import gi
from PIL import Image

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, GLib  # noqa: E402


def interface_kind(text):
    labels = "".join(character for character in text.lower() if character.isalnum())
    # A dialog can leave background workspace controls readable to OCR/AT-SPI.
    # Give known overlays precedence over those controls as a second check.
    if (any(word in labels for word in ("getstarted", "开始使用"))
            and any(word in labels for word in ("addapikey", "addanapikey", "添加apikey"))
            and any(word in labels for word in ("signin", "登录"))):
        return "optional-account"
    groups = {
        "onboarding": (
            "addanapikeytogetstarted", "添加一个apikey开始使用", "configurelater", "稍后配置",
        ),
        "preview": ("previewnotice", "预览须知", "预览说明", "预览提示", "预览公告"),
        "workspace": (
            "newsession", "newchat", "新会话", "新建会话", "whatcanihelpyouwith",
            "addworkspace", "添加工作区",
            "describewhatyouwanttobuild", "描述你想要构建的内容",
        ),
    }
    for kind, words in groups.items():
        if any(word in labels for word in words):
            return kind
    return None


def geometry(window):
    result = subprocess.check_output(["xwininfo", "-id", window], text=True)
    fields = {}
    for key, pattern in {
        "x": r"Absolute upper-left X:\s*(-?\d+)",
        "y": r"Absolute upper-left Y:\s*(-?\d+)",
        "width": r"\bWidth:\s*(\d+)",
        "height": r"\bHeight:\s*(\d+)",
    }.items():
        match = re.search(pattern, result)
        if match is None:
            raise RuntimeError("Could not determine the client window bounds")
        fields[key] = int(match.group(1))
    if fields["x"] < 0 or fields["y"] < 0 or fields["width"] < 600 or fields["height"] < 400:
        raise RuntimeError("The visible client window has invalid bounds")
    return fields


def capture_interface(window, output):
    bounds = geometry(window)
    area = ",".join(str(bounds[name]) for name in ("x", "y", "width", "height"))
    subprocess.run(["scrot", "--overwrite", "--autoselect", area, str(output)], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
    # Only OCR DSH's content: the client's local toolbar must never satisfy this check.
    with Image.open(output) as image:
        region = image.crop((0, max(60, int(image.height * 0.11)), image.width, image.height))
        region = region.resize((region.width * 2, region.height * 2))
        with tempfile.TemporaryDirectory(prefix="dsh-window-ocr-") as scratch:
            crop = Path(scratch) / "content.png"
            region.save(crop)
            text = subprocess.check_output(
                ["tesseract", str(crop), "stdout", "-l", "eng+chi_sim", "--psm", "11"],
                stderr=subprocess.DEVNULL, text=True, timeout=20,
            )
    return interface_kind(text)


def wait_for_interface(pid, window, output, dismissed_dialogs):
    deadline = time.monotonic() + 60
    unobstructed_since = None
    while time.monotonic() < deadline:
        os.kill(pid, 0)
        kind = capture_interface(window, output)
        try:
            page = dsh_page_snapshot(pid, geometry(window))
            if page is not None:
                clear, dismissed = dismiss_optional_dialog(page["dialogs"])
                if dismissed is not None:
                    dismissed_dialogs.append(dismissed)
                if clear and kind == "workspace":
                    if unobstructed_since is None:
                        unobstructed_since = time.monotonic()
                    # First-run account state may arrive after the composer.
                    # Do not pass through a newly arriving modal overlay.
                    if time.monotonic() - unobstructed_since >= 2:
                        return kind, page["document"]["bounds"]
                else:
                    unobstructed_since = None
            else:
                unobstructed_since = None
        except GLib.Error:
            # Painting can finish before the AT-SPI document is available.
            unobstructed_since = None
        time.sleep(0.5)
    raise RuntimeError("The actual client window did not show an unobstructed DSH workspace with a correctly sized native document")


def matches_content_bounds(document, window):
    if (document.get("title") != "DeepSeek Harness"
            or not document.get("visible") or not document.get("showing")):
        return False
    bounds = document.get("bounds", {})
    expected = {"x": window["x"], "y": window["y"] + 40,
                "width": window["width"], "height": window["height"] - 40}
    # Xvfb/GTK scale is fixed to one. Allow only small native border/rounding
    # differences, never a zero-height child packed below the visible window.
    return all(name in bounds and abs(bounds[name] - value) <= 4
               for name, value in expected.items())


def is_toolbar_control(node, bounds):
    component = node.get_component_iface()
    if component is None:
        return False
    rect = component.get_extents(Atspi.CoordType.SCREEN)
    # The smoke session fixes GTK scale to 1. Match the trusted shell's 40 px
    # toolbar, never a similarly named button inside the separate DSH WebView.
    return (rect.width > 0 and rect.height > 0
            and rect.x >= bounds["x"] and rect.x + rect.width <= bounds["x"] + bounds["width"]
            and rect.y >= bounds["y"] and rect.y + rect.height <= bounds["y"] + 40)


def document_diagnostics(pid):
    """Only document roles, allowlisted titles and geometry; never page text."""
    Atspi.init()
    if hasattr(Atspi, "set_timeout"):
        Atspi.set_timeout(500, 1000)
    context = GLib.MainContext.default()
    while context.pending():
        context.iteration(False)
    desktop = Atspi.get_desktop(0)
    queue = deque()
    for index in range(desktop.get_child_count()):
        app = desktop.get_child_at_index(index)
        if app is not None and app.get_process_id() == pid:
            queue.append(app)
    documents = []
    document_roles = {
        getattr(Atspi.Role, name, None)
        for name in ("DOCUMENT_WEB", "DOCUMENT_FRAME", "DOCUMENT_TEXT")
    }
    deadline = time.monotonic() + 8
    visited = 0
    while queue and visited < 2000 and time.monotonic() < deadline:
        node = queue.popleft()
        visited += 1
        try:
            role = node.get_role()
            if role in document_roles:
                title = node.get_name() or ""
                states = node.get_state_set()
                row = {
                    "role": node.get_role_name(),
                    "title": title if title in ("DeepSeekHarness", "DeepSeek Harness") else "<other or empty>",
                    "visible": states.contains(Atspi.StateType.VISIBLE),
                    "showing": states.contains(Atspi.StateType.SHOWING),
                }
                component = node.get_component_iface()
                if component is not None:
                    rect = component.get_extents(Atspi.CoordType.SCREEN)
                    row["bounds"] = {name: getattr(rect, name) for name in ("x", "y", "width", "height")}
                documents.append(row)
            for index in range(node.get_child_count()):
                child = node.get_child_at_index(index)
                if child is not None:
                    queue.append(child)
        except GLib.Error:
            continue
    return {"documents": documents, "accessible_nodes_visited": visited}


def document_nodes(root):
    queue = deque([root])
    visited = 0
    deadline = time.monotonic() + 8
    while queue and visited < 2000 and time.monotonic() < deadline:
        node = queue.popleft()
        visited += 1
        yield node
        try:
            for index in range(node.get_child_count()):
                child = node.get_child_at_index(index)
                if child is not None:
                    queue.append(child)
        except GLib.Error:
            continue


def dsh_page_snapshot(pid, bounds):
    """Inspect only the correctly positioned remote DSH document, not its shell."""
    Atspi.init()
    if hasattr(Atspi, "set_timeout"):
        Atspi.set_timeout(500, 1000)
    context = GLib.MainContext.default()
    while context.pending():
        context.iteration(False)
    desktop = Atspi.get_desktop(0)
    roles = {getattr(Atspi.Role, name, None) for name in ("DOCUMENT_WEB", "DOCUMENT_FRAME")}
    for index in range(desktop.get_child_count()):
        app = desktop.get_child_at_index(index)
        if app is None or app.get_process_id() != pid:
            continue
        for node in document_nodes(app):
            try:
                if node.get_role() not in roles or node.get_name() != "DeepSeek Harness":
                    continue
                states = node.get_state_set()
                component = node.get_component_iface()
                if component is None:
                    continue
                rect = component.get_extents(Atspi.CoordType.SCREEN)
                document = {
                    "title": "DeepSeek Harness",
                    "visible": states.contains(Atspi.StateType.VISIBLE),
                    "showing": states.contains(Atspi.StateType.SHOWING),
                    "bounds": {name: getattr(rect, name) for name in ("x", "y", "width", "height")},
                }
                if not matches_content_bounds(document, bounds):
                    continue
                dialogs = []
                for descendant in document_nodes(node):
                    role = descendant.get_role()
                    state = descendant.get_state_set()
                    if (role == Atspi.Role.DIALOG or state.contains(Atspi.StateType.MODAL)) \
                            and state.contains(Atspi.StateType.SHOWING):
                        dialogs.append(list(document_nodes(descendant)))
                return {"document": document, "dialogs": dialogs}
            except GLib.Error:
                continue
    return None


def optional_dialog_choice(names):
    labels = {"".join(character for character in name.lower() if character.isalnum()) for name in names}
    if (labels.intersection(("getstarted", "开始使用"))
            and labels.intersection(("addapikey", "添加apikey"))
            and labels.intersection(("signin", "登录"))):
        return "optional-account", ("close", "关闭")
    if labels.intersection(("previewnotice", "预览版说明", "预览须知", "预览说明", "预览提示", "预览公告")):
        return "preview", ("continue", "继续")
    if labels.intersection(("addanapikeytogetstarted", "添加一个apikey开始使用")):
        return "optional-api-key", ("configurelater", "稍后配置")
    return None


def dismiss_optional_dialog(dialogs):
    if not dialogs:
        return True, None
    # Portaled/nested dialogs are appended after their background document.
    # An unknown visible modal blocks success; never invoke background controls.
    nodes = dialogs[-1]
    names = [node.get_name() or "" for node in nodes]
    choice = optional_dialog_choice(names)
    if choice is None:
        return False, None
    kind, safe_buttons = choice
    for node in nodes:
        state = node.get_state_set()
        name = "".join(character for character in (node.get_name() or "").lower() if character.isalnum())
        if (node.get_role() == Atspi.Role.PUSH_BUTTON and name in safe_buttons
                and state.contains(Atspi.StateType.SHOWING)
                and (state.contains(Atspi.StateType.ENABLED) or state.contains(Atspi.StateType.SENSITIVE))):
            action = node.get_action_iface()
            if action is not None and action.get_n_actions() > 0 and action.do_action(0):
                return False, kind
    return False, None


def refresh_client(pid, window):
    Atspi.init()
    if hasattr(Atspi, "set_timeout"):
        Atspi.set_timeout(1500, 3000)
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        bounds = geometry(window)
        context = GLib.MainContext.default()
        while context.pending():
            context.iteration(False)
        desktop = Atspi.get_desktop(0)
        applications = []
        for index in range(desktop.get_child_count()):
            app = desktop.get_child_at_index(index)
            if app is not None and app.get_process_id() == pid:
                applications.append(app)
        queue = deque(applications)
        visited = 0
        while queue and visited < 1500:
            node = queue.popleft()
            visited += 1
            try:
                states = node.get_state_set()
                if (node.get_role() == Atspi.Role.PUSH_BUTTON
                        and node.get_name() in ("Refresh", "刷新页面")
                        and states.contains(Atspi.StateType.SHOWING)
                        and (states.contains(Atspi.StateType.ENABLED)
                             or states.contains(Atspi.StateType.SENSITIVE))
                        and is_toolbar_control(node, bounds)):
                    action = node.get_action_iface()
                    if action is not None and action.get_n_actions() > 0 and action.do_action(0):
                        return
                for index in range(node.get_child_count()):
                    child = node.get_child_at_index(index)
                    if child is not None:
                        queue.append(child)
            except GLib.Error:
                # Accessibility objects can be replaced while WebKit renders.
                continue
        time.sleep(0.5)
    raise RuntimeError("Could not invoke the real client Refresh button through AT-SPI")


def resize_content_edges(window):
    """Drag the actual child WebView's right and bottom edges, without WM shortcuts."""
    evidence = []
    for edge in ("right", "bottom"):
        before = geometry(window)
        if edge == "right":
            start = (before["x"] + before["width"] - 2, before["y"] + before["height"] // 2)
            end = (start[0] - 80, start[1])
            expected = {**before, "width": before["width"] - 80}
        else:
            start = (before["x"] + before["width"] // 2, before["y"] + before["height"] - 2)
            end = (start[0], start[1] - 60)
            expected = {**before, "height": before["height"] - 60}
        subprocess.run([
            "xdotool", "mousemove", "--sync", *(str(value) for value in start),
            "mousedown", "1", "sleep", "0.15", "mousemove", "--sync",
            *(str(value) for value in end), "sleep", "0.3", "mouseup", "1",
        ], check=True, timeout=10)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            after = geometry(window)
            if all(abs(after[name] - expected[name]) <= 4 for name in expected):
                evidence.append({"edge": edge, "before": before, "after": after})
                break
            time.sleep(0.2)
        else:
            raise RuntimeError("Dragging the actual DSH content's " + edge + " edge did not resize the window")
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pid", type=int)
    parser.add_argument("window")
    parser.add_argument("screenshot", type=Path)
    args = parser.parse_args()
    args.screenshot.parent.mkdir(parents=True, exist_ok=True)
    windows = subprocess.check_output(["wmctrl", "-lp"], text=True).splitlines()
    if not any(int(row.split()[0], 16) == int(args.window, 16)
               and int(row.split()[2]) == args.pid for row in windows):
        raise RuntimeError("Capture window does not belong to the test app")
    subprocess.run(["wmctrl", "-ir", args.window, "-e", "0,0,0,1600,1000"], check=True)
    subprocess.run(["wmctrl", "-ia", args.window], check=True)
    try:
        dialogs_before = []
        dialogs_after = []
        before, before_bounds = wait_for_interface(args.pid, args.window, args.screenshot, dialogs_before)
        resize_evidence = resize_content_edges(args.window)
        refresh_client(args.pid, args.window)
        # Give the real toolbar navigation time to replace the previous document.
        time.sleep(2)
        after, after_bounds = wait_for_interface(args.pid, args.window, args.screenshot, dialogs_after)
    except Exception:
        try:
            diagnostics = document_diagnostics(args.pid)
            diagnostics["window_bounds"] = geometry(args.window)
        except Exception as error:
            diagnostics = {"diagnostic_error_type": type(error).__name__}
        debug_path = args.screenshot.with_suffix(".page-debug.json")
        debug_path.write_text(json.dumps(diagnostics, indent=2) + "\n")
        print("Linux page diagnostics: " + json.dumps(diagnostics), file=sys.stderr, flush=True)
        raise
    evidence = {"result": "passed", "actual_dsh_document": True,
                "rendered_dsh_before_refresh": before, "rendered_dsh_after_refresh": after,
                "dsh_document_bounds_before_refresh": before_bounds,
                "dsh_document_bounds_after_refresh": after_bounds,
                "unobstructed_workspace_before_refresh": True,
                "unobstructed_workspace_after_refresh": True,
                "optional_dialogs_dismissed_before_refresh": dialogs_before,
                "optional_dialogs_dismissed_after_refresh": dialogs_after,
                "content_edge_resize": resize_evidence,
                "toolbar_refresh": "AT-SPI action passed", "screenshot": str(args.screenshot)}
    args.screenshot.with_suffix(".page.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence), flush=True)


if __name__ == "__main__":
    main()
