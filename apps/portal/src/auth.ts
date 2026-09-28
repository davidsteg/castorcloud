import NextAuth from "next-auth";
import Authentik from "next-auth/providers/authentik";
import { prisma } from "@pc/db";
import { BASE_DOMAIN, PUBLIC_SCHEME } from "@/lib/config";

const SESSION_MAX_AGE_S = 30 * 24 * 60 * 60;

export const { handlers, auth, signIn, signOut } = NextAuth({
  // The portal is served on app.<domain> and every <slug>.<domain>.
  trustHost: true,
  // Same as Authentik's session_duration (authentik/blueprints/30-sessions.yaml),
  // so embedded services can still sign in silently while the portal is signed in.
  session: { maxAge: SESSION_MAX_AGE_S },
  providers: [
    Authentik({
      clientId: process.env.PORTAL_OIDC_CLIENT_ID,
      clientSecret: process.env.PORTAL_OIDC_CLIENT_SECRET,
      issuer: `${PUBLIC_SCHEME}://auth.${BASE_DOMAIN}/application/o/portal/`,
    }),
  ],
  callbacks: {
    async jwt({ token, profile }) {
      if (profile?.sub) {
        token.sub = profile.sub;
        token.authAt = Date.now();
        return token;
      }
      // "Log out on all devices" (lib/signout.ts) ends sessions signed in before it.
      if (token.sub) {
        const user = await prisma.user.findUnique({ where: { authentikSub: token.sub }, select: { sessionsRevokedAt: true } });
        const authAt = typeof token.authAt === "number" ? token.authAt : 0;
        if (user?.sessionsRevokedAt && authAt < user.sessionsRevokedAt.getTime()) return null;
      }
      return token;
    },
    session({ session, token }) {
      if (token.sub) session.user.id = token.sub;
      return session;
    },
  },
  events: {
    // Mirror Authentik users into the control-plane DB.
    async signIn({ profile }) {
      if (!profile?.sub || !profile.email) return;
      await prisma.user.upsert({
        where: { authentikSub: profile.sub },
        update: { email: profile.email, name: profile.name ?? null },
        create: { authentikSub: profile.sub, email: profile.email, name: profile.name ?? null },
      });
    },
  },
});
