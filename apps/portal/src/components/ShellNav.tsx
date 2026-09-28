"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { Logo } from "@/components/site/SiteHeader";

export type ShellNavItem = { href: string; label: string; icon: string };

export function ShellNav({ brand, tenantName, email, items, signOut }: {
  brand: string;
  tenantName: string;
  email: string;
  items: ShellNavItem[];
  signOut: { label: string; action: () => Promise<void> };
}) {
  const pathname = usePathname();

  return (
    <nav className="shell-nav">
      <div className="shell-brand" title={brand}><Logo size={26} /></div>
      {items.map((item) => {
        const active = item.href === "/" ? pathname === "/" : pathname.startsWith(item.href);
        return (
          <Link key={item.href} href={item.href} className="shell-link" aria-current={active ? "page" : undefined}>
            <span aria-hidden>{item.icon}</span>
            {item.label}
          </Link>
        );
      })}
      <form action={signOut.action}>
        <button type="submit" className="shell-link">
          <span aria-hidden>⏻</span>
          {signOut.label}
        </button>
      </form>
      <div className="shell-footer">
        <div>{tenantName}</div>
        <div>{email}</div>
      </div>
    </nav>
  );
}
