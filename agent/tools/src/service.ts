import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import { ToolError } from "./limits.ts";
import type { Member } from "./members.ts";

/** Same keys as the portal and the provisioner use for booked services. */
export type ServiceKey = "photos" | "docs" | "places" | "flights";

export type ToolContext = {
  member: Member;
  /** e.g. https://muster.castorcloud.ch; results link back into the portal. */
  portalOrigin: string;
};

export type ServiceModule = {
  key: ServiceKey;
  /** False when the service is not booked (its env is not set). */
  configured: boolean;
  register(server: McpServer, ctx: ToolContext): void;
};

/** Every tool is read-only; Hermes uses the hint to skip write approvals. */
export const READ_ONLY = { readOnlyHint: true, destructiveHint: false, openWorldHint: false } as const;

/** Runs a tool; expected failures become a short message for the model. */
export async function answer(run: () => Promise<unknown>): Promise<CallToolResult> {
  try {
    const result = await run();
    return { content: [{ type: "text", text: JSON.stringify(result) }] };
  } catch (err) {
    if (err instanceof ToolError) return { isError: true, content: [{ type: "text", text: err.message }] };
    console.error(err);
    return { isError: true, content: [{ type: "text", text: "internal error in the tool" }] };
  }
}
