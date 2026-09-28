-- AlterEnum
ALTER TYPE "JobKind" ADD VALUE 'RESTORE';

-- AlterTable
ALTER TABLE "Job" ADD COLUMN "params" JSONB;

-- CreateTable
CREATE TABLE "TenantBackup" (
    "tenantId" TEXT NOT NULL,
    "generation" INTEGER NOT NULL DEFAULT 0,
    "lastSnapshot" TEXT,
    "lastBytes" BIGINT,
    "lastSuccessAt" TIMESTAMP(3),
    "lastAttemptAt" TIMESTAMP(3),
    "lastError" TEXT,
    "failures" INTEGER NOT NULL DEFAULT 0,
    "lastVerifiedAt" TIMESTAMP(3),
    "verifyError" TEXT,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "TenantBackup_pkey" PRIMARY KEY ("tenantId")
);

-- AddForeignKey
ALTER TABLE "TenantBackup" ADD CONSTRAINT "TenantBackup_tenantId_fkey" FOREIGN KEY ("tenantId") REFERENCES "Tenant"("id") ON DELETE CASCADE ON UPDATE CASCADE;
