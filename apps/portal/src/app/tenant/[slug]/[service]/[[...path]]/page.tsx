import { notFound } from "next/navigation";
import { isServiceKey, serviceOrigin } from "@/lib/config";
import { ServiceFrame } from "@/components/ServiceFrame";
import { requireTenantMember } from "@/lib/tenant";

export default async function ServicePage({
  params,
}: {
  params: Promise<{ slug: string; service: string; path?: string[] }>;
}) {
  const { slug, service, path = [] } = await params;
  if (!isServiceKey(service)) notFound();
  const { enabledServices } = await requireTenantMember(slug);
  if (!enabledServices.includes(service)) notFound();

  return (
    <ServiceFrame
      service={service}
      origin={serviceOrigin(slug, service)}
      initialPath={"/" + path.map(encodeURIComponent).join("/")}
    />
  );
}
