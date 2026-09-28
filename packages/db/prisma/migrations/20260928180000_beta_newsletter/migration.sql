-- AlterEnum
ALTER TYPE "TenantStatus" ADD VALUE 'APPLIED' BEFORE 'PENDING';

-- AlterTable
ALTER TABLE "Tenant" ADD COLUMN "applicationNote" TEXT,
ADD COLUMN "appliedAt" TIMESTAMP(3),
ADD COLUMN "reviewedAt" TIMESTAMP(3),
ADD COLUMN "reviewedBy" TEXT;

-- CreateEnum
CREATE TYPE "NewsletterStatus" AS ENUM ('PENDING', 'CONFIRMED', 'UNSUBSCRIBED');

-- CreateTable
CREATE TABLE "NewsletterSubscriber" (
    "id" TEXT NOT NULL,
    "email" TEXT NOT NULL,
    "locale" TEXT NOT NULL DEFAULT 'de',
    "status" "NewsletterStatus" NOT NULL DEFAULT 'PENDING',
    "confirmTokenHash" TEXT,
    "confirmSentAt" TIMESTAMP(3),
    "source" TEXT NOT NULL,
    "confirmedAt" TIMESTAMP(3),
    "unsubscribedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "NewsletterSubscriber_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "NewsletterSubscriber_email_key" ON "NewsletterSubscriber"("email");

-- CreateIndex
CREATE UNIQUE INDEX "NewsletterSubscriber_confirmTokenHash_key" ON "NewsletterSubscriber"("confirmTokenHash");
