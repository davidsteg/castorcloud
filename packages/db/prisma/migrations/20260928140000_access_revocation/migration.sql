-- AlterEnum
ALTER TYPE "JobKind" ADD VALUE 'REVOKE_ACCESS';

-- CreateEnum
CREATE TYPE "RevocationScope" AS ENUM ('WEB', 'DEVICES', 'REMOVED');

-- AlterTable
ALTER TABLE "User" ADD COLUMN "sessionsRevokedAt" TIMESTAMP(3);

-- CreateTable
CREATE TABLE "AccessRevocation" (
    "id" TEXT NOT NULL,
    "tenantId" TEXT NOT NULL,
    "email" TEXT NOT NULL,
    "sub" TEXT NOT NULL,
    "scope" "RevocationScope" NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "AccessRevocation_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE INDEX "AccessRevocation_tenantId_idx" ON "AccessRevocation"("tenantId");

-- AddForeignKey
ALTER TABLE "AccessRevocation" ADD CONSTRAINT "AccessRevocation_tenantId_fkey" FOREIGN KEY ("tenantId") REFERENCES "Tenant"("id") ON DELETE CASCADE ON UPDATE CASCADE;
