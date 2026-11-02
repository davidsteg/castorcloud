"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useId, useRef, useState } from "react";

type MenuLink = { href: string; label: string };

/**
 * Hamburger menu for narrow screens, where the header nav is hidden.
 * Closes on navigation, Escape and a click outside; focus returns to the button.
 */
export function MobileMenu({
  label,
  links,
  login,
  start,
  locales,
}: {
  label: string;
  links: MenuLink[];
  login: MenuLink;
  start: MenuLink;
  locales: (MenuLink & { current: boolean })[];
}) {
  const [open, setOpen] = useState(false);
  const pathname = usePathname();
  const panelId = useId();
  const buttonRef = useRef<HTMLButtonElement>(null);
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(() => setOpen(false), [pathname]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setOpen(false);
        buttonRef.current?.focus();
      }
    };
    const onClick = (e: MouseEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("click", onClick);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("click", onClick);
    };
  }, [open]);

  return (
    <div className="mobile-menu" ref={rootRef}>
      <button
        ref={buttonRef}
        type="button"
        className="menu-toggle"
        aria-expanded={open}
        aria-controls={panelId}
        aria-label={label}
        onClick={() => setOpen((v) => !v)}
      >
        <span aria-hidden className="menu-icon" />
      </button>
      <nav id={panelId} className="menu-panel" hidden={!open} aria-label={label}>
        {links.map((l) => (
          <Link key={l.href} href={l.href} onClick={() => setOpen(false)}>
            {l.label}
          </Link>
        ))}
        <div className="menu-actions">
          <Link className="button small" href={start.href} onClick={() => setOpen(false)}>
            {start.label}
          </Link>
          <Link className="button secondary small" href={login.href} onClick={() => setOpen(false)}>
            {login.label}
          </Link>
        </div>
        <div className="menu-langs">
          {locales.map((l) => (
            <Link key={l.href} href={l.href} aria-current={l.current} onClick={() => setOpen(false)}>
              {l.label}
            </Link>
          ))}
        </div>
      </nav>
    </div>
  );
}
