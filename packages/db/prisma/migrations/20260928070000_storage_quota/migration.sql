-- AlterTable
ALTER TABLE "Tenant" ADD COLUMN     "storageByService" JSONB,
ADD COLUMN     "storageMeasuredAt" TIMESTAMP(3),
ADD COLUMN     "storageUsedMb" INTEGER,
ADD COLUMN     "storageVolumeMb" INTEGER;
