/** Username of the Castor Telegram bot (without @); unset = Telegram off. */
export const TELEGRAM_BOT = process.env.TELEGRAM_BOT_USERNAME?.replace(/^@/, "") || null;
