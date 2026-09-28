-- CreateEnum
CREATE TYPE "NodeKind" AS ENUM ('LOCAL', 'MAC');

-- AlterTable
ALTER TABLE "Node" ADD COLUMN     "authHostIp" TEXT,
ADD COLUMN     "kind" "NodeKind" NOT NULL DEFAULT 'MAC',
ADD COLUMN     "lastError" TEXT,
ADD COLUMN     "lastSeenAt" TIMESTAMP(3),
ADD COLUMN     "memoryMb" INTEGER,
ADD COLUMN     "poolHealth" TEXT,
ADD COLUMN     "poolSizeMb" INTEGER,
ADD COLUMN     "poolUsedMb" INTEGER,
ADD COLUMN     "zfsPool" TEXT;


-- The only node before this migration is the local development host.
UPDATE "Node" SET "kind" = 'LOCAL' WHERE "sshHost" = 'local';
