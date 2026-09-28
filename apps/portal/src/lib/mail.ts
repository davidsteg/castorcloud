import "server-only";
import nodemailer from "nodemailer";
import { BASE_DOMAIN, BRAND_NAME } from "@/lib/config";

// Outgoing e-mail of the portal (applications, newsletter confirmations).
// Any SMTP provider works (MAIL_SMTP_URL); another transport only needs to
// implement Mailer. Without MAIL_SMTP_URL, development prints mails to the
// console and production refuses to send.

export type Mail = { to: string; subject: string; text: string; headers?: Record<string, string> };

export interface Mailer {
  send(mail: Mail): Promise<void>;
}

/** Only over TLS: smtps:// (implicit) or smtp:// with STARTTLS required. */
function smtpMailer(url: string, from: string): Mailer {
  const u = new URL(url);
  if (u.protocol !== "smtps:" && u.protocol !== "smtp:") throw new Error("MAIL_SMTP_URL must be smtps:// or smtp://");
  const secure = u.protocol === "smtps:";
  const transport = nodemailer.createTransport({
    host: u.hostname,
    port: Number(u.port || (secure ? 465 : 587)),
    secure,
    requireTLS: true,
    tls: { minVersion: "TLSv1.2" },
    auth: u.username ? { user: decodeURIComponent(u.username), pass: decodeURIComponent(u.password) } : undefined,
  });
  return {
    async send(mail) {
      await transport.sendMail({ from, to: mail.to, subject: mail.subject, text: mail.text, headers: mail.headers });
    },
  };
}

const consoleMailer: Mailer = {
  async send(mail) {
    if (process.env.NODE_ENV === "production") throw new Error("MAIL_SMTP_URL is not set: cannot send e-mail");
    console.info(`[mail] to ${mail.to}: ${mail.subject}\n${mail.text}`);
  },
};

let mailer: Mailer | undefined;

export function getMailer(): Mailer {
  mailer ??= process.env.MAIL_SMTP_URL
    ? smtpMailer(process.env.MAIL_SMTP_URL, process.env.MAIL_FROM || `${BRAND_NAME} <hello@${BASE_DOMAIN}>`)
    : consoleMailer;
  return mailer;
}

/** Sends without failing the caller: returns whether the mail went out. */
export async function trySend(mail: Mail): Promise<boolean> {
  try {
    await getMailer().send(mail);
    return true;
  } catch (err) {
    console.error(`[mail] "${mail.subject}" not sent: ${(err as Error).message}`);
    return false;
  }
}
