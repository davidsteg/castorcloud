import NextAuth from "next-auth";
import Authentik from "next-auth/providers/authentik";
import { prisma } from "@pc/db";
import { BASE_DOMAIN, PUBLIC_SCHEME } from "@/lib/config";

export const { handlers, auth, signIn, signOut } = NextAuth({
  // The portal is served on app.<domain> and every <slug>.<domain>.
  trustHost: true,
  providers: [
    Authentik({
      clientId: process.env.PORTAL_OIDC_CLIENT_ID,
      clientSecret: process.env.PORTAL_OIDC_CLIENT_SECRET,
      issuer: `${PUBLIC_SCHEME}://auth.${BASE_DOMAIN}/application/o/portal/`,
    }),
  ],
  callbacks: {
    jwt({ token, profile }) {
      if (profile?.sub) token.sub = profile.sub;
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
