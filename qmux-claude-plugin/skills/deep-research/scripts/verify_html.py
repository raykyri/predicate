#!/usr/bin/env python3
"""
HTML Report Verification Script
Validates that HTML reports are properly generated with all sections from MD
"""

import argparse
import re
from pathlib import Path
from typing import List, Tuple


class HTMLVerifier:
    """Verify HTML research reports"""

    def __init__(self, html_path: Path, md_path: Path):
        self.html_path = html_path
        self.md_path = md_path
        self.errors = []
        self.warnings = []

    def verify(self) -> bool:
        """
        Run all verification checks

        Returns:
            True if all checks pass, False otherwise
        """
        print(f"\n{'='*60}")
        print(f"HTML REPORT VERIFICATION")
        print(f"{'='*60}\n")

        print(f"HTML File: {self.html_path}")
        print(f"MD File: {self.md_path}\n")

        # Read files
        try:
            html_content = self.html_path.read_text()
            md_content = self.md_path.read_text()
        except Exception as e:
            self.errors.append(f"Failed to read files: {e}")
            return False

        # Run checks
        self._check_sections(html_content, md_content)
        self._check_no_placeholders(html_content)
        self._check_no_injected_scripts(html_content)
        self._check_no_emojis(html_content)
        self._check_structure(html_content)
        self._check_citations(html_content, md_content)
        self._check_bibliography(html_content, md_content)

        # Report results
        self._print_results()

        return len(self.errors) == 0

    def _check_sections(self, html: str, md: str):
        """Verify all markdown sections are present in HTML"""
        # Extract section headings from markdown
        md_sections = re.findall(r'^## (.+)$', md, re.MULTILINE)

        # Extract sections from HTML
        html_sections = re.findall(r'<h2 class="section-title">(.+?)</h2>', html)

        # Check if we have placeholder sections like <div class="section">#</div>
        placeholder_sections = re.findall(r'<div class="section">#</div>', html)

        if placeholder_sections:
            self.errors.append(
                f"Found {len(placeholder_sections)} placeholder sections (empty '#' divs) - content not converted properly"
            )

        # Compare section counts
        if len(md_sections) > len(html_sections) + 1:  # +1 for bibliography which is separate
            self.errors.append(
                f"Section count mismatch: MD has {len(md_sections)} sections, HTML has only {len(html_sections)} + bibliography"
            )
            missing = set(md_sections) - set(html_sections)
            if missing:
                self.errors.append(f"Missing sections in HTML: {missing}")

        # Verify Executive Summary is present
        if "Executive Summary" in md and "Executive Summary" not in html:
            self.errors.append("Executive Summary missing from HTML")

    def _check_no_placeholders(self, html: str):
        """Check for common placeholders that shouldn't be in final report"""
        placeholders = [
            '{{TITLE}}', '{{DATE}}', '{{CONTENT}}', '{{BIBLIOGRAPHY}}',
            '{{METRICS_DASHBOARD}}', '{{SOURCE_COUNT}}', 'TODO', 'TBD',
            'PLACEHOLDER', 'FIXME'
        ]

        found = []
        for placeholder in placeholders:
            if placeholder in html:
                found.append(placeholder)

        if found:
            self.errors.append(f"Found unreplaced placeholders: {', '.join(found)}")

    def _check_no_injected_scripts(self, html: str):
        """Flag executable markup that must never reach the auto-opened report.

        The HTML is opened by the user's default application as a local file:// page, and its
        content originates from untrusted web sources. Any of these patterns means
        the markdown->HTML escaping in md_to_html.py failed (or was bypassed); the
        report must NOT be opened until fixed. Because content is HTML-escaped at
        conversion time, a clean report never matches these — every match is a real
        injection signal, not legitimate prose (which appears as &lt;script&gt;).
        """
        injection_patterns = [
            (re.compile(r'<\s*script', re.IGNORECASE), '<script> tag'),
            (re.compile(r'<\s*iframe', re.IGNORECASE), '<iframe> tag'),
            (re.compile(r'<\s*object', re.IGNORECASE), '<object> tag'),
            (re.compile(r'<\s*embed', re.IGNORECASE), '<embed> tag'),
            # javascript: only matters inside an attribute (href/src/action);
            # as plain prose or a cited URL it is inert, so don't flag that.
            (re.compile(r'(?:href|src|action|formaction)\s*=\s*["\']?\s*javascript:',
                        re.IGNORECASE), 'javascript: URI in attribute'),
            # An on*= event handler inside an opening tag (anchored to "<tag ..."
            # so it cannot false-positive on ordinary prose like "on x = y").
            (re.compile(r'<\s*\w+[^>]*\son\w+\s*=', re.IGNORECASE | re.DOTALL),
             'inline event handler (on*=)'),
        ]

        found = []
        for pattern, description in injection_patterns:
            if pattern.search(html):
                found.append(description)

        if found:
            self.errors.append(
                "Potential script injection in HTML — escaping failed, do NOT open "
                f"this report: {', '.join(found)}"
            )

    def _check_no_emojis(self, html: str):
        """Verify no emojis are present in HTML"""
        # Common emoji patterns
        emoji_pattern = re.compile(
            "["
            "\U0001F600-\U0001F64F"  # emoticons
            "\U0001F300-\U0001F5FF"  # symbols & pictographs
            "\U0001F680-\U0001F6FF"  # transport & map symbols
            "\U0001F1E0-\U0001F1FF"  # flags
            "\U00002702-\U000027B0"
            "\U000024C2-\U0001F251"
            "]+",
            flags=re.UNICODE
        )

        emojis = emoji_pattern.findall(html)
        if emojis:
            unique_emojis = set(emojis)
            self.errors.append(f"Found {len(emojis)} emojis in HTML (should be none): {unique_emojis}")

    def _check_structure(self, html: str):
        """Verify HTML has proper structure"""
        required_elements = [
            ('<html', 'HTML tag'),
            ('<head', 'head tag'),
            ('<body', 'body tag'),
            ('<title>', 'title tag'),
            ('class="header"', 'header section'),
            ('class="content"', 'content section'),
            ('class="bibliography"', 'bibliography section'),
        ]

        for element, name in required_elements:
            if element not in html:
                self.errors.append(f"Missing {name} in HTML")

        # Check for unclosed tags (basic check)
        open_divs = html.count('<div')
        close_divs = html.count('</div>')

        if abs(open_divs - close_divs) > 2:  # Allow small discrepancy
            self.warnings.append(
                f"Possible unclosed divs: {open_divs} opening tags, {close_divs} closing tags"
            )

    def _check_citations(self, html: str, md: str):
        """Verify citations are present"""
        # Extract citations from markdown
        md_citations = set(re.findall(r'\[(\d+)\]', md))

        # Extract citations from HTML (excluding bibliography)
        html_content = html.split('class="bibliography"')[0] if 'class="bibliography"' in html else html
        html_citations = set(re.findall(r'\[(\d+)\]', html_content))

        if len(md_citations) > 0 and len(html_citations) == 0:
            self.errors.append("No citations found in HTML content (but present in MD)")

        if len(md_citations) > len(html_citations) * 1.5:  # Allow some variation
            self.warnings.append(
                f"Fewer citations in HTML ({len(html_citations)}) than MD ({len(md_citations)})"
            )

    def _check_bibliography(self, html: str, md: str):
        """Verify bibliography is present and formatted"""
        if '## Bibliography' in md:
            if 'class="bibliography"' not in html:
                self.errors.append("Bibliography section missing from HTML")
            elif 'class="bib-entry"' not in html:
                self.warnings.append("Bibliography present but entries not properly formatted")

    def _print_results(self):
        """Print verification results"""
        print(f"\n{'-'*60}")
        print("VERIFICATION RESULTS")
        print(f"{'-'*60}\n")

        if self.errors:
            print(f"❌ ERRORS ({len(self.errors)}):")
            for i, error in enumerate(self.errors, 1):
                print(f"  {i}. {error}")
            print()

        if self.warnings:
            print(f"⚠️  WARNINGS ({len(self.warnings)}):")
            for i, warning in enumerate(self.warnings, 1):
                print(f"  {i}. {warning}")
            print()

        if not self.errors and not self.warnings:
            print("✅ All checks passed! HTML report is valid.")
            print()

        print(f"{'-'*60}\n")


def main():
    """Main entry point"""
    parser = argparse.ArgumentParser(description='Verify HTML research report')
    parser.add_argument('--html', type=Path, required=True, help='Path to HTML report')
    parser.add_argument('--md', type=Path, required=True, help='Path to markdown report')

    args = parser.parse_args()

    if not args.html.exists():
        print(f"Error: HTML file not found: {args.html}")
        return 1

    if not args.md.exists():
        print(f"Error: Markdown file not found: {args.md}")
        return 1

    verifier = HTMLVerifier(args.html, args.md)
    success = verifier.verify()

    return 0 if success else 1


if __name__ == "__main__":
    exit(main())
