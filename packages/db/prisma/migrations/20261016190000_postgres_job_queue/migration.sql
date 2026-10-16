-- Tenant lifecycle jobs move from BullMQ (Redis) to this table; see
-- packages/core/src/jobs.ts for how workers claim them.

-- AlterTable
ALTER TABLE "Job" ADD COLUMN     "attempts" INTEGER NOT NULL DEFAULT 0,
ADD COLUMN     "lockedBy" TEXT,
ADD COLUMN     "lockedUntil" TIMESTAMP(3),
ADD COLUMN     "maxAttempts" INTEGER NOT NULL DEFAULT 5,
ADD COLUMN     "runAfter" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP;

-- CreateTable
CREATE TABLE "WorkerHeartbeat" (
    "id" TEXT NOT NULL,
    "kinds" "JobKind"[],
    "seenAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "WorkerHeartbeat_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE INDEX "Job_status_runAfter_idx" ON "Job"("status", "runAfter");

-- CreateIndex
CREATE INDEX "Job_tenantId_status_idx" ON "Job"("tenantId", "status");


-- CreateTable
CREATE TABLE "Schedule" (
    "name" TEXT NOT NULL,
    "nextRunAt" TIMESTAMP(3) NOT NULL,
    "lastRunAt" TIMESTAMP(3),

    CONSTRAINT "Schedule_pkey" PRIMARY KEY ("name")
);

-- A job left RUNNING by the old worker has no lease and no BullMQ entry
-- any more: fail it visibly instead of silently re-running it.
UPDATE "Job"
SET "status" = 'FAILED',
    "error" = 'interrupted when the job queue moved to Postgres; enqueue it again if still needed',
    "finishedAt" = now() AT TIME ZONE 'UTC'
WHERE "status" = 'RUNNING';

-- Jobs of one tenant never overlap, whichever worker runs them.
CREATE UNIQUE INDEX "Job_one_running_per_tenant" ON "Job"("tenantId") WHERE "status" = 'RUNNING';
