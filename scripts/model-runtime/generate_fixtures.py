#!/usr/bin/env python3
"""Generate deterministic UI screenshots for local VLM acceptance tests."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


WIDTH = 1280
HEIGHT = 720


def font(size: int, bold: bool = False) -> ImageFont.ImageFont:
    windows_fonts = Path("C:/Windows/Fonts")
    name = "segoeuib.ttf" if bold else "segoeui.ttf"
    candidate = windows_fonts / name
    if candidate.is_file():
        return ImageFont.truetype(str(candidate), size=size)
    return ImageFont.load_default()


def centered(draw: ImageDraw.ImageDraw, box: tuple[int, int, int, int], text: str, size: int) -> None:
    selected = font(size, bold=True)
    left, top, right, bottom = draw.textbbox((0, 0), text, font=selected)
    width = right - left
    height = bottom - top
    x = box[0] + (box[2] - box[0] - width) // 2
    y = box[1] + (box[3] - box[1] - height) // 2
    draw.text((x, y), text, font=selected, fill="#ffffff")


def base_window() -> tuple[Image.Image, ImageDraw.ImageDraw]:
    image = Image.new("RGB", (WIDTH, HEIGHT), "#eef1f4")
    draw = ImageDraw.Draw(image)
    draw.rectangle((0, 0, WIDTH, 58), fill="#20252b")
    draw.text((28, 15), "ALE MODEL RUNTIME TEST", font=font(24, bold=True), fill="#ffffff")
    draw.rectangle((0, 58, 220, HEIGHT), fill="#303740")
    items = ["Overview", "Models", "Runtime", "Reports"]
    for index, item in enumerate(items):
        y = 96 + index * 58
        if item == "Models":
            draw.rectangle((16, y - 10, 204, y + 34), fill="#47515d")
        draw.text((34, y), item, font=font(18), fill="#ffffff")
    draw.text((260, 92), "Local model packages", font=font(30, bold=True), fill="#1b1f23")
    draw.text(
        (260, 140),
        "Pinned models are ready for the Windows runtime acceptance test.",
        font=font(17),
        fill="#4b5560",
    )
    return image, draw


def unique_target(path: Path) -> dict[str, object]:
    image, draw = base_window()
    cards = [
        (260, 205, 560, 360, "Qwen2.5-VL-7B", "Planner"),
        (590, 205, 890, 360, "ShowUI-2B", "Grounder"),
        (920, 205, 1220, 360, "UI-TARS-1.5-7B", "Fallback"),
    ]
    for left, top, right, bottom, title, role in cards:
        draw.rounded_rectangle((left, top, right, bottom), radius=6, fill="#ffffff", outline="#c5cbd2", width=2)
        draw.text((left + 22, top + 24), title, font=font(19, bold=True), fill="#1f2933")
        draw.text((left + 22, top + 66), role, font=font(16), fill="#5f6b76")
        draw.text((left + 22, top + 106), "READY", font=font(15, bold=True), fill="#177245")

    button = (880, 560, 1160, 638)
    draw.rounded_rectangle(button, radius=6, fill="#147d50", outline="#0e633e", width=2)
    centered(draw, button, "DOWNLOAD MODELS", 20)
    draw.text((260, 585), "Action", font=font(18, bold=True), fill="#343b43")
    image.save(path, format="PNG")
    return {
        "file": path.name,
        "target": "DOWNLOAD MODELS button",
        "bbox_pixels": list(button),
        "bbox_normalized": [button[0] / WIDTH, button[1] / HEIGHT, button[2] / WIDTH, button[3] / HEIGHT],
    }


def ambiguous_target(path: Path) -> dict[str, object]:
    image, draw = base_window()
    background_save = (980, 570, 1160, 632)
    draw.rounded_rectangle(background_save, radius=6, fill="#69727c")
    centered(draw, background_save, "SAVE", 19)
    overlay = Image.new("RGBA", (WIDTH, HEIGHT), (0, 0, 0, 0))
    overlay_draw = ImageDraw.Draw(overlay)
    overlay_draw.rectangle((220, 58, WIDTH, HEIGHT), fill=(25, 30, 36, 155))
    image = Image.alpha_composite(image.convert("RGBA"), overlay).convert("RGB")
    draw = ImageDraw.Draw(image)

    dialog = (370, 145, 1010, 620)
    draw.rounded_rectangle(dialog, radius=8, fill="#ffffff", outline="#9aa3ad", width=2)
    draw.text((408, 180), "Settings", font=font(28, bold=True), fill="#1f252b")
    draw.text((408, 238), "Runtime backend", font=font(17), fill="#4b5560")
    draw.rounded_rectangle((408, 272, 972, 330), radius=4, fill="#f2f4f6", outline="#bec5cc")
    draw.text((430, 289), "Vulkan - AMD Radeon Pro WX 9100", font=font(17), fill="#252b31")
    draw.text((408, 370), "Unload idle models after 120 seconds", font=font(17), fill="#252b31")
    draw.rectangle((914, 366, 966, 394), fill="#147d50")

    cancel = (650, 520, 800, 578)
    target_save = (818, 520, 968, 578)
    draw.rounded_rectangle(cancel, radius=5, fill="#68727c")
    draw.rounded_rectangle(target_save, radius=5, fill="#147d50")
    centered(draw, cancel, "CANCEL", 17)
    centered(draw, target_save, "SAVE", 17)
    image.save(path, format="PNG")
    return {
        "file": path.name,
        "target": "SAVE button inside the Settings dialog",
        "bbox_pixels": list(target_save),
        "bbox_normalized": [
            target_save[0] / WIDTH,
            target_save[1] / HEIGHT,
            target_save[2] / WIDTH,
            target_save[3] / HEIGHT,
        ],
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    expected = {
        "image_size": [WIDTH, HEIGHT],
        "unique": unique_target(args.output_dir / "unique-target.png"),
        "ambiguous": ambiguous_target(args.output_dir / "ambiguous-target.png"),
    }
    (args.output_dir / "expected.json").write_text(
        json.dumps(expected, indent=2, sort_keys=True), encoding="utf-8"
    )


if __name__ == "__main__":
    main()
