-- Custom stacks: customers' own apps on separate STACKS nodes (docs/stacks.md).
ALTER TYPE "JobKind" ADD VALUE 'STACKS';

CREATE TYPE "NodeRole" AS ENUM ('TENANTS', 'STACKS');
CREATE TYPE "StackSource" AS ENUM ('CATALOG', 'CUSTOM');
CREATE TYPE "StackStatus" AS ENUM ('PENDING', 'DEPLOYING', 'RUNNING', 'STOPPED', 'FAILED', 'REMOVING');

ALTER TABLE "Node"
  ADD COLUMN "role" "NodeRole" NOT NULL DEFAULT 'TENANTS',
  ADD COLUMN "cpus" INTEGER,
  ADD COLUMN "stackRuntime" TEXT,
  ADD COLUMN "uidBase" INTEGER,
  ADD COLUMN "controlPlaneIps" TEXT;

ALTER TABLE "Tenant"
  ADD COLUMN "stacksNodeId" TEXT,
  ADD COLUMN "stackMemoryUnits" INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN "stackCpuUnits" INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN "stackStorageUnits" INTEGER NOT NULL DEFAULT 0;

ALTER TABLE "Tenant" ADD CONSTRAINT "Tenant_stacksNodeId_fkey" FOREIGN KEY ("stacksNodeId") REFERENCES "Node"("id") ON DELETE SET NULL ON UPDATE CASCADE;

CREATE TABLE "Stack" (
    "id" TEXT NOT NULL,
    "tenantId" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "source" "StackSource" NOT NULL,
    "catalogKey" TEXT,
    "definition" TEXT NOT NULL,
    "memoryMb" INTEGER NOT NULL,
    "cpus" DOUBLE PRECISION NOT NULL,
    "storageGb" INTEGER NOT NULL,
    "status" "StackStatus" NOT NULL DEFAULT 'PENDING',
    "error" TEXT,
    "images" JSONB,
    "deployedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "Stack_pkey" PRIMARY KEY ("id")
);

CREATE UNIQUE INDEX "Stack_tenantId_name_key" ON "Stack"("tenantId", "name");

ALTER TABLE "Stack" ADD CONSTRAINT "Stack_tenantId_fkey" FOREIGN KEY ("tenantId") REFERENCES "Tenant"("id") ON DELETE CASCADE ON UPDATE CASCADE;
