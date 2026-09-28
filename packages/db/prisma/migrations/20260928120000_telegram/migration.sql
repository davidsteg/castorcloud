-- CreateTable
CREATE TABLE "TelegramLink" (
    "id" TEXT NOT NULL,
    "memberId" TEXT NOT NULL,
    "pairingCodeHash" TEXT,
    "pairingExpiresAt" TIMESTAMP(3),
    "telegramUserId" BIGINT,
    "telegramUsername" TEXT,
    "sessionId" TEXT,
    "linkedAt" TIMESTAMP(3),
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,

    CONSTRAINT "TelegramLink_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "TelegramLink_memberId_key" ON "TelegramLink"("memberId");

-- CreateIndex
CREATE UNIQUE INDEX "TelegramLink_pairingCodeHash_key" ON "TelegramLink"("pairingCodeHash");

-- CreateIndex
CREATE UNIQUE INDEX "TelegramLink_telegramUserId_key" ON "TelegramLink"("telegramUserId");

-- AddForeignKey
ALTER TABLE "TelegramLink" ADD CONSTRAINT "TelegramLink_memberId_fkey" FOREIGN KEY ("memberId") REFERENCES "Member"("id") ON DELETE CASCADE ON UPDATE CASCADE;

