import Image from "next/image";
import { OSS_PROJECTS, SERVICES, SERVICE_KEYS, type OssProject } from "@pc/pricing";
import { fill, type Dictionary } from "@/i18n";
import { LOGIN_SCREENSHOT, SCREENSHOTS } from "@/lib/screenshots";

/** The open-source projects with their real screenshots, source and licence. */
export function OssGallery({ t, compact = false }: { t: Dictionary; compact?: boolean }) {
  const o = t.openSourcePage;
  const projects = Object.keys(OSS_PROJECTS) as OssProject[];
  const role = (project: OssProject) => {
    const service = SERVICE_KEYS.find((s) => SERVICES[s].project === project);
    return service ? t.services.items[service].name : "";
  };

  return (
    <div className={compact ? "oss-gallery compact" : "oss-gallery"}>
      {projects.map((key) => {
        const shot = SCREENSHOTS[key];
        const project = OSS_PROJECTS[key];
        return (
          <figure className="oss-card" key={key}>
            <a className="oss-shot" href={project.url} target="_blank" rel="noreferrer">
              {shot ? (
                <Image src={shot.src} alt={`${project.name} – ${role(key)}`} width={shot.width} height={shot.height}
                  sizes={compact ? "(max-width: 640px) 100vw, 360px" : "(max-width: 960px) 100vw, 560px"} />
              ) : key === "hermes" ? (
                // No official screenshot: the sample chat as it appears in the cloud.
                <span className="oss-chat" aria-hidden>
                  <span className="bubble user">{t.showcase.sample.question}</span>
                  <span className="bubble bot">{t.showcase.sample.answer}</span>
                </span>
              ) : (
                // No screenshot yet.
                <span className="oss-chat" aria-hidden>
                  <span className="bubble bot">{t.services.items[SERVICE_KEYS.find((s) => SERVICES[s].project === key) ?? "files"].text}</span>
                </span>
              )}
            </a>
            <figcaption>
              <b>{project.name}</b>
              <span className="muted">{role(key)}</span>
              {!compact && (
                <small>
                  {shot ? (
                    <a href={shot.source} target="_blank" rel="noreferrer">
                      {fill(o.credit, { name: project.name, license: shot.license })}
                    </a>
                  ) : (
                    o.noScreenshot
                  )}
                </small>
              )}
            </figcaption>
          </figure>
        );
      })}
      {/* The shared sign-in page, on the full open-source page only. */}
      {!compact && (
        <figure className="oss-card">
          <a className="oss-shot" href={LOGIN_SCREENSHOT.url} target="_blank" rel="noreferrer">
            <Image src={LOGIN_SCREENSHOT.src} alt={`${LOGIN_SCREENSHOT.name} – ${o.login}`}
              width={LOGIN_SCREENSHOT.width} height={LOGIN_SCREENSHOT.height}
              sizes="(max-width: 960px) 100vw, 560px" />
          </a>
          <figcaption>
            <b>{LOGIN_SCREENSHOT.name}</b>
            <span className="muted">{o.login}</span>
            <small>
              <a href={LOGIN_SCREENSHOT.source} target="_blank" rel="noreferrer">
                {fill(o.credit, { name: LOGIN_SCREENSHOT.name, license: LOGIN_SCREENSHOT.license })}
              </a>
            </small>
          </figcaption>
        </figure>
      )}
    </div>
  );
}
