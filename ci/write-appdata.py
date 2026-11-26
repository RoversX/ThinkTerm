"""Write the Linux package's AppStream metadata with its release version and date."""

import sys
import xml.etree.ElementTree as ET
from pathlib import Path


version, release_date, destination = sys.argv[1:]
metadata = ET.parse(Path(__file__).resolve().parents[1] / "assets/wezterm.appdata.xml")
releases = ET.SubElement(metadata.getroot(), "releases")
ET.SubElement(releases, "release", version=version, date=release_date)

output = Path(destination)
output.parent.mkdir(parents=True, exist_ok=True)
metadata.write(output, encoding="utf-8", xml_declaration=True)
output.chmod(0o644)
