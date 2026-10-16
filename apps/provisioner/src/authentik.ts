// Minimal Authentik API client for what the provisioner needs:
// tenant groups, one OIDC provider + application per tenant service,
// and policy bindings that restrict each application to the tenant's group.
// All operations are idempotent (look up by name/slug, then create or update).

type Page<T> = { results: T[] };
type Pk = { pk: number | string };

export type RedirectUri = { matching_mode: "strict" | "regex"; url: string };

export class Authentik {
  constructor(
    private readonly baseUrl: string,
    private readonly token: string,
  ) {}

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await fetch(`${this.baseUrl}/api/v3${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${this.token}`,
        "Content-Type": "application/json",
        Accept: "application/json",
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (res.status === 204) return undefined as T;
    const text = await res.text();
    if (!res.ok) throw new Error(`Authentik ${method} ${path} -> ${res.status}: ${text}`);
    return (text ? JSON.parse(text) : undefined) as T;
  }

  // Note: /core/applications/ only lists apps the caller may access unless
  // superuser_full_list=true is passed (tenant apps are bound to tenant groups).
  private async findOne<T>(path: string, query: Record<string, string>): Promise<T | undefined> {
    const qs = new URLSearchParams(query).toString();
    const page = await this.request<Page<T>>("GET", `${path}?${qs}`);
    return page.results[0];
  }

  private async mustFind<T>(what: string, path: string, query: Record<string, string>): Promise<T> {
    const found = await this.findOne<T>(path, query);
    if (!found) throw new Error(`Authentik: ${what} not found (${JSON.stringify(query)})`);
    return found;
  }

  /** Flows, scope mappings and signing key shared by every provider we create. */
  async providerDefaults() {
    const [authz, invalidation, openid, email, profile, key] = await Promise.all([
      this.mustFind<Pk>("authorization flow", "/flows/instances/", { slug: "default-provider-authorization-implicit-consent" }),
      this.mustFind<Pk>("invalidation flow", "/flows/instances/", { slug: "default-provider-invalidation-flow" }),
      this.mustFind<Pk>("openid scope", "/propertymappings/provider/scope/", { managed: "goauthentik.io/providers/oauth2/scope-openid" }),
      this.mustFind<Pk>("email scope", "/propertymappings/provider/scope/", { managed: "goauthentik.io/providers/oauth2/scope-email" }),
      this.mustFind<Pk>("profile scope", "/propertymappings/provider/scope/", { managed: "goauthentik.io/providers/oauth2/scope-profile" }),
      this.mustFind<Pk>("signing key", "/crypto/certificatekeypairs/", { name: "authentik Self-signed Certificate" }),
    ]);
    return {
      authorization_flow: authz.pk,
      invalidation_flow: invalidation.pk,
      property_mappings: [openid.pk, email.pk, profile.pk],
      signing_key: key.pk,
    };
  }

  async ensureGroup(name: string): Promise<string> {
    const existing = await this.findOne<Pk>("/core/groups/", { name });
    if (existing) return String(existing.pk);
    const created = await this.request<Pk>("POST", "/core/groups/", { name, is_superuser: false });
    return String(created.pk);
  }

  async findUserPk(username: string): Promise<number> {
    const user = await this.mustFind<{ pk: number }>("user", "/core/users/", { username });
    return user.pk;
  }

  /**
   * Instantiates a blueprint shipped with Authentik (e.g. an example flow),
   * waits until the flow it creates exists, then re-applies `reapply`
   * (a blueprint that references that flow). Applying is asynchronous.
   */
  async ensureBlueprintInstance(name: string, path: string, flowSlug: string, reapply?: string) {
    const existing = await this.findOne<{ pk: string; path: string }>("/managed/blueprints/", { name });
    const instance = existing
      ? await this.request<{ pk: string }>("PATCH", `/managed/blueprints/${existing.pk}/`, { path, enabled: true })
      : await this.request<{ pk: string }>("POST", "/managed/blueprints/", { name, path, enabled: true, context: {} });
    await this.request("POST", `/managed/blueprints/${instance.pk}/apply/`);
    for (let i = 0; i < 30 && !(await this.findOne("/flows/instances/", { slug: flowSlug })); i++) {
      await new Promise((r) => setTimeout(r, 2000));
    }
    if (reapply) {
      const other = await this.findOne<{ pk: string }>("/managed/blueprints/", { name: reapply });
      if (other) await this.request("POST", `/managed/blueprints/${other.pk}/apply/`);
    }
  }

  async findUserPkByEmail(email: string): Promise<number | undefined> {
    const user = await this.findOne<{ pk: number }>("/core/users/", { email });
    return user?.pk;
  }

  async addUserToGroup(groupPk: string, userPk: number) {
    await this.request("POST", `/core/groups/${groupPk}/add_user/`, { pk: userPk });
  }

  /** Members of a group; `uid` is the OIDC "sub" (providers use hashed_user_id). */
  async groupUsers(groupPk: string): Promise<{ pk: number; email: string; uid: string }[]> {
    const group = await this.request<{ users_obj: { pk: number; email: string; uid: string }[] }>(
      "GET", `/core/groups/${groupPk}/?include_users=true`,
    );
    return group.users_obj ?? [];
  }

  async removeUserFromGroup(groupPk: string, userPk: number) {
    await this.request("POST", `/core/groups/${groupPk}/remove_user/`, { pk: userPk });
  }

  /** Signs the user out of Authentik in every browser and drops their refresh tokens. */
  async endUserSessions(userPk: number) {
    const user = await this.request<{ username: string }>("GET", `/core/users/${userPk}/`);
    const sessions = await this.request<Page<{ uuid: string }>>(
      "GET", `/core/authenticated_sessions/?${new URLSearchParams({ user__username: user.username, page_size: "1000" })}`,
    );
    for (const s of sessions.results) await this.request("DELETE", `/core/authenticated_sessions/${s.uuid}/`);
    const tokens = await this.request<Page<{ id: number }>>("GET", `/oauth2/refresh_tokens/?user=${userPk}&page_size=1000`);
    for (const t of tokens.results) await this.request("DELETE", `/oauth2/refresh_tokens/${t.id}/`);
  }

  async deleteGroup(name: string) {
    const group = await this.findOne<Pk>("/core/groups/", { name });
    if (group) await this.request("DELETE", `/core/groups/${group.pk}/`);
  }

  /** Removes an application and its provider, so the service can't be signed in to. */
  async deleteOidcApp(slug: string) {
    const app = await this.findOne<{ slug: string }>("/core/applications/", { slug, superuser_full_list: "true" });
    if (app) await this.request("DELETE", `/core/applications/${slug}/`);
    const provider = await this.findOne<{ pk: number }>("/providers/oauth2/", { name: slug });
    if (provider) await this.request("DELETE", `/providers/oauth2/${provider.pk}/`);
  }

  /** Creates or updates a confidential OIDC provider and its application. */
  async ensureOidcApp(opts: {
    slug: string;
    name: string;
    clientId: string;
    clientSecret: string;
    redirectUris: RedirectUri[];
    launchUrl: string;
    allowedGroupPk: string;
    defaults: Awaited<ReturnType<Authentik["providerDefaults"]>>;
  }): Promise<{ providerPk: number; issuer: string }> {
    const providerBody = {
      name: opts.slug,
      client_type: "confidential",
      client_id: opts.clientId,
      client_secret: opts.clientSecret,
      redirect_uris: opts.redirectUris,
      sub_mode: "hashed_user_id",
      include_claims_in_id_token: true,
      ...opts.defaults,
    };
    const existing = await this.findOne<{ pk: number }>("/providers/oauth2/", { name: opts.slug });
    const provider = existing
      ? await this.request<{ pk: number }>("PATCH", `/providers/oauth2/${existing.pk}/`, providerBody)
      : await this.request<{ pk: number }>("POST", "/providers/oauth2/", providerBody);

    const appBody = { name: opts.name, slug: opts.slug, provider: provider.pk, meta_launch_url: opts.launchUrl };
    const app = (await this.findOne<{ pk: string }>("/core/applications/", { slug: opts.slug, superuser_full_list: "true" }))
      ?? (await this.request<{ pk: string }>("POST", "/core/applications/", appBody));
    await this.request("PATCH", `/core/applications/${opts.slug}/`, appBody);

    // Only members of the tenant group may use this application.
    const binding = await this.findOne<Pk>("/policies/bindings/", { target: app.pk, group: opts.allowedGroupPk });
    if (!binding) {
      await this.request("POST", "/policies/bindings/", { target: app.pk, group: opts.allowedGroupPk, order: 0, enabled: true });
    }

    return { providerPk: provider.pk, issuer: `${this.baseUrl}/application/o/${opts.slug}/` };
  }

  // ---------------------------------------------------------- custom stacks
  // Custom stacks get no OIDC client: Authentik's embedded outpost signs
  // people in in front of them (Traefik forward auth, "forward_single" mode,
  // one provider per stack host), and only the tenant's group gets through.

  private async embeddedOutpost() {
    return this.mustFind<{ pk: string; providers: number[] }>("embedded outpost", "/outposts/instances/", {
      managed__iexact: "goauthentik.io/outposts/embedded",
    });
  }

  /** Creates or updates the forward-auth provider and application of one stack host. */
  async ensureProxyApp(opts: {
    slug: string;
    name: string;
    externalHost: string;
    allowedGroupPk: string;
    defaults: Awaited<ReturnType<Authentik["providerDefaults"]>>;
  }) {
    const providerBody = {
      name: opts.slug,
      mode: "forward_single",
      external_host: opts.externalHost,
      authorization_flow: opts.defaults.authorization_flow,
      invalidation_flow: opts.defaults.invalidation_flow,
      // The app sees who is signed in (X-authentik-*), never a token it could replay elsewhere.
      intercept_header_auth: false,
      basic_auth_enabled: false,
      access_token_validity: "hours=12",
    };
    const existing = await this.findOne<{ pk: number }>("/providers/proxy/", { name: opts.slug });
    const provider = existing
      ? await this.request<{ pk: number }>("PATCH", `/providers/proxy/${existing.pk}/`, providerBody)
      : await this.request<{ pk: number }>("POST", "/providers/proxy/", providerBody);

    const appBody = { name: opts.name, slug: opts.slug, provider: provider.pk, meta_launch_url: opts.externalHost };
    const app = (await this.findOne<{ pk: string }>("/core/applications/", { slug: opts.slug, superuser_full_list: "true" }))
      ?? (await this.request<{ pk: string }>("POST", "/core/applications/", appBody));
    await this.request("PATCH", `/core/applications/${opts.slug}/`, appBody);
    const binding = await this.findOne<Pk>("/policies/bindings/", { target: app.pk, group: opts.allowedGroupPk });
    if (!binding) {
      await this.request("POST", "/policies/bindings/", { target: app.pk, group: opts.allowedGroupPk, order: 0, enabled: true });
    }

    const outpost = await this.embeddedOutpost();
    if (!outpost.providers.includes(provider.pk)) {
      await this.request("PATCH", `/outposts/instances/${outpost.pk}/`, { providers: [...outpost.providers, provider.pk] });
    }
  }

  async deleteProxyApp(slug: string) {
    const provider = await this.findOne<{ pk: number }>("/providers/proxy/", { name: slug });
    if (provider) {
      const outpost = await this.embeddedOutpost();
      if (outpost.providers.includes(provider.pk)) {
        await this.request("PATCH", `/outposts/instances/${outpost.pk}/`, { providers: outpost.providers.filter((p) => p !== provider.pk) });
      }
    }
    const app = await this.findOne<{ slug: string }>("/core/applications/", { slug, superuser_full_list: "true" });
    if (app) await this.request("DELETE", `/core/applications/${slug}/`);
    if (provider) await this.request("DELETE", `/providers/proxy/${provider.pk}/`);
  }
}
